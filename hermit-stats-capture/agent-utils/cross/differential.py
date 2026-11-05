#!/usr/bin/env python3
"""Python-vs-Rust differential tester for dagrun.

Prove the Python and Rust builds produce identical OBSERVABLE behavior. For a set of
representative and randomized DAG fixtures, this runs BOTH the Python CLI (``python3 -m dagrun``)
and a private copy of the executable resolved by the tracked ``rs/bin`` Cargo launcher and
asserts:

* ``list``, ``ascii``, ``dot`` stdout are BYTE-IDENTICAL.
* ``json`` stdout is BYTE-IDENTICAL (both builds emit ``ensure_ascii=False`` canonical JSON, so
  the bytes match for every input — including multi-line / quote / backslash / control-char /
  unicode ``description`` fields, and floats in scientific notation).
* Every ``.yaml`` fixture is ISOMORPHIC to JSON: loaded in BOTH builds and re-emitted as
  canonical JSON, the bytes match; and each ``examples/NAME.{json,yaml}`` pair loads to the same
  DAG.
* A shared nested JSON/YAML ``include`` fixture flattens to byte-identical namespaced output,
  including resource-cap unions and parent-owned ``after`` entry-node wiring.
* Scalar-resolution PARITY: a battery of adversarial scalars (the YAML "Norway problem",
  octal/underscore/sexagesimal/timestamp tokens, non-finite and overflow floats, and
  out-of-i64-range integers) is loaded in BOTH builds and must yield the SAME accept/reject exit
  code — and byte-identical JSON whenever both accept. This catches PyYAML-1.1-vs-serde_norway-1.2
  and Python-json-vs-serde_json divergences that the isomorphism check (which only covers
  documents both builds accept) cannot.
* ``run`` agrees on exit code, and on the passed/failed/aborted/intentionally-skipped/
  dependency-skipped/not-launched counts. Counts are
  compared under ``--keep-going`` so they are deterministic (the default eager-exit path
  races on which in-flight step is cancelled first, so only its exit code is compared).
* ``--selected`` selection agrees: a named leaf brings in its full dependency ancestry, while
  ``--ignore-selected-deps`` runs only the named node. Both builds match on exit code and counts,
  and reject an unknown selected tag or the dependency-ignoring flag without a selection.
  A successful ``sweep`` must produce the same width rows and table schema; measured timing cells
  and the ``--profile`` table are not byte-compared because runtimes legitimately differ.
* The memory-aware ``--max-steps`` decision and modeled footprint from ``--max-mem`` match,
  including effective-width scaling, hard/default/engine-only runnable steps, prompt refusal when
  even one step cannot fit, CPA infeasibility driven by learned RSS, and signed-64 saturation.
* ``run`` keeps active-step fan-out (``-s``) independent from its total CPU budget
  (``-j`` / ``--max-cpus``): the same fork-based guest workload records normalized step/worker
  overlap under both engines, including two full-width steps whose aggregate live workers exceed
  the budget while each individual width remains bounded. A declared over-budget self-managed
  width is refused before spawn, and profile-derived recommendations shown by every run planner
  remain within the same per-step ceiling. A capability-gated boxed case additionally proves that
  this allowed worker oversubscription remains inside the live outer ``cpu.max`` and long-window
  ``cpu.stat`` bandwidth envelope.
* The auto-logging profile STORE (Feature D) has an identical on-disk schema across builds: an
  unboxed run under each build (into separate ``--perf-dir`` dirs) writes the SAME set of CSV
  filenames — so ``machine_id`` + ``container_class`` (and hence ``nproc``) agree — with
  byte-identical HEADER rows and the SAME line-ending style. Data rows (timestamps, elapsed, git
  SHA) legitimately differ and are not compared. The dynamic cgroup ``cpu.*`` columns only appear
  under boxing (out of scope for the unboxed differential); their alphabetical ordering is pinned
  by each build's own perflog tests. Each reader also builds byte-identical typed summaries from
  the other implementation's freshly emitted store, and simultaneous mixed-language writers must
  preserve every expected whole-run and per-step row.
* The opt-in time-series TRACE has one cross-language schema and encoding: on hosts that can box a
  step, a one-width sweep under each build writes the same fixed columns plus the same sorted sweep
  provenance tail. Volatile timestamps, run ids, counters and timings are shape-normalized, while
  field precision, blank-vs-present semantics, sample ordering and stable provenance are compared.
  Hosts without the required cgroup-v2 scope take the same loud capability-skip path.
* The ``sweep`` ``--jobs`` error text (malformed range / not-an-integer) matches across builds, and
  a step with an empty effective ``jobs_flag`` is refused because its guest width cannot vary.
* The profile-store FEEDBACK loop + ``--planner`` agree (``compare_plan_feedback``): against a FIXED
  synthetic store, ``plan`` output is byte-identical across builds for BOTH planners and BOTH formats
  (so the contention-discounted median durations, high-percentile rss estimates, and dispatch order
  all match); the ``critical-path`` order differs from ``greedy-lpt`` (the planner really reorders);
  the hint-only ``--no-profile-feedback`` plan is also identical; and the ``--max-mem`` sizing fed by
  the store's rss estimates matches across builds and throttles below the CPU count.
* The deterministic ``scaling_model_*.json`` sidecar rebuilt from one fixed raw profile store is
  byte-identical across builds, including workload digests and the trailing newline.
* The standalone interactive sweep report embeds the same schema-1 historical dataset in both
  builds when generated from the same raw rows. Presentation markup may evolve independently;
  the graph, samples, filters, fitted-input data, traces, and profiler-capture manifests may not
  diverge.
* The remaining ``run`` comparisons pass ``--no-profile`` (no store WRITE into the harness CWD) and
  ``--no-profile-feedback`` (no store READ / hint refinement), so the base scheduling behavior under
  test stays hermetic and hint-only; feedback parity is asserted separately (above).

It also keeps the bootstrap ``--version`` / ``--help`` / no-args exit-code checks, and asserts
``--userguide`` stdout is BYTE-IDENTICAL across builds (both embed the same single-source guide).

A check that cannot run on the current host or lane (no cgroup-v2 boxing, a parent-owned delegated
cgroup that must not spawn live scopes, a refused HARD cpuset pin, fewer than two usable CPUs, or a
core budget below the eight CPUs the CPA memory-cap check needs) is recorded as SKIPPED, a result
class of its own: it is never counted as a pass, and each skipped check is listed by name with a
total. Where both engines refuse a capability identically, that parity is counted as a pass and the
check the capability would have enabled is still listed as skipped.

Exit status is nonzero on any divergence, and also on any skip unless the run declared that kind of
skip acceptable with ``--allow-skip KIND`` or ``AGENT_UTILS_CROSS_ALLOW_SKIP=KIND[,KIND]``. Such a
run is partial by declaration; it says so on its first line and ends with ``PARTIAL`` instead of
``OK``. The module is kept mypy-strict clean.
"""

from __future__ import annotations

import argparse
import csv
import fcntl
import hashlib
import json
import os
import select
import signal
import random
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Mapping, Sequence
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

from cpu_footprint_analysis import (
    FootprintStats,
    analyze as analyze_cpu_footprint,
    check_cgroup_bandwidth,
    check_limits as check_cpu_limits,
    load_events as load_cpu_events,
)
from herdr_agent_differential import compare_herdr_agent
from agentctl_differential import compare_agentctl
from herdr_differential import SKIP_KINDS as HERDR_RUN_SKIP_KINDS
from herdr_differential import build_report as build_herdr_run_report

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

#: Passed to every generic ``run`` comparison.  Those cases test scheduling semantics rather than
#: cgroup setup, so select the deterministic unboxed core explicitly.  The weaker
#: ``--allow-cgroup-failure`` is not equivalent on a capable or already-delegated host: it can
#: remain boxed and make the harness's physical limits part of a synthetic fixture.  Cases that
#: exercise boxing omit this flag and explicitly retain the outer runner authority instead.
UNBOXED = "--unsafe-no-cgroups"

#: Passed to every `run` comparison so the default auto-logging profile store (Feature D) does not
#: write CSVs into the harness CWD during the differential; the scheduling behavior under test is
#: independent of profile logging, which is proven by each build's own tests.
NOPROF = "--no-profile"

#: Passed to the existing `run` comparisons so plan-time profile-store FEEDBACK (the learned-estimate
#: reader) does NOT read the repo-local default store and silently refine hints mid-differential —
#: the base scheduling behavior under test must stay hint-only and hermetic. Feedback parity is
#: proven separately by :func:`compare_plan_feedback` against a fixed SYNTHETIC store.
NOFB = "--no-profile-feedback"

#: Feedback identity envs (mirrors dagrun.estimates): pin the machine id + container class so the
#: feedback reader loads a fixed synthetic ``step_profiles_<mid>_<cc>.csv`` regardless of the host,
#: making the plan/sizing feedback checks deterministic everywhere.
SYNTH_MACHINE = "cross_synth_machine"
SYNTH_CONTAINER = "cross_synth_container"

#: The one-hop nesting handshake granted to THIS differential by an outer dagrun.  It is not
#: ambient configuration for the Python/Rust children it compares.
#: Letting a generic child inherit them makes a synthetic ``--max-mem 64G`` planning case run as
#: a nested scheduler inside this harness's much smaller step cgroup, so both engines correctly
#: refuse the fixture and the differential reports dozens of false failures.  Explicit boxing
#: cases opt back in via ``inherit_runner_authority=True`` so they remain below the parent-owned
#: containment boundary rather than asking systemd to move into a sibling scope.  Ancestor-scope
#: identity and expected-limit variables are deliberately NOT here: in a delegated child they
#: describe the ancestor, while the delegated root itself is the live source of physical limits.
_RUNNER_AUTHORITY_ENV = frozenset(
    {
        "DAGRUN_DELEGATED_CGROUP",
        "DAGRUN_DELEGATED_UNBOXED",
        "DAGRUN_OUTER_RUN",
    }
)

#: Runner inputs that each synthetic case must state itself.  Unlike the authority set above,
#: these are scrubbed even for a boxed case; an individual fixture can reintroduce one in ``extra``
#: after the scrub when that variable is the behavior under test.  ``DAGRUN_STEP`` and
#: ``DAGRUN_STEP_STARTED_MONOTONIC_NS`` are deliberately absent: they preserve the outer runner's
#: process-tree ownership and deadline attribution across the harness's subprocess boundary.
_AMBIENT_CASE_CONTROL_ENV = frozenset(
    {
        "DAGRUN_ADMISSION_BUDGET_BYTES",
        "DAGRUN_ADMISSION_HEADROOM_BYTES",
        "AGENT_UTILS_VALIDATION_JOBS",
        "DAGRUN_CAPTURE_MAX_BYTES",
        "DAGRUN_CONTAINER_CLASS",
        "DAGRUN_COPIES",
        "DAGRUN_COPY",
        "DAGRUN_CORE_LEDGER",
        "DAGRUN_CPU_TIMEOUT_MULTIPLIER",
        "DAGRUN_CPU_TIMEOUT_PLATFORM",
        "DAGRUN_DIRECT_CGROUP",
        "DAGRUN_EXTRA_ARGS",
        "DAGRUN_EXPECTED_OUTER_CPU_COUNT",
        "DAGRUN_EXPECTED_OUTER_MEMORY_MAX_BYTES",
        "DAGRUN_EXPECTED_RUNTIME_MAX_SEC",
        "DAGRUN_FORCE_SCOPE_ATTEMPT",
        "DAGRUN_IN_SCOPE",
        "DAGRUN_JOBS_ENV",
        "DAGRUN_LOG_DIR",
        "DAGRUN_LOG_MAX_BYTES",
        "DAGRUN_MEM_LEDGER",
        "DAGRUN_NO_STEP_LOGS",
        "DAGRUN_OUTER_MEMORY_MAX_BYTES",
        "DAGRUN_PROFILE_DIR",
        "DAGRUN_RESOURCE_CAPS_PATH",
        "DAGRUN_RUN_TIMEOUT",
        "DAGRUN_SCOPE_UNIT",
        "DAGRUN_TEST_COUNTS_PATH",
    }
)

_COUNTS_RE = re.compile(
    r"(\d+) passed, (\d+) failed, (\d+) aborted, "
    r"(\d+) intentionally skipped, (\d+) dependency-skipped, (\d+) not launched"
)
_SIZING_RE = re.compile(
    r"-> modeled memory ceiling (\d+) active steps "
    r"\(worst-case (\d+) bytes fits budget (\d+) bytes\); "
    r"base active-step ceiling (\d+); final --max-steps (\d+)"
)
_SIZING_REFUSAL_RE = re.compile(
    r"REFUSED — minimum runnable footprint (\d+) bytes cannot fit safely within budget "
    r"(\d+) bytes"
)
_TERMINAL_STEP_DURATION_RE = re.compile(
    r"(?m)^(\[[^\]\n]+\] [✓✗] (?:PASS|FAIL)\s+.+?) "
    r"\([0-9]+s(, [^\n)]*)?\)$"
)

CPU_FOOTPRINT_GUEST = os.path.join(REPO_ROOT, "cross", "cpu_footprint_guest.py")

#: The validation graph uses dagrun's ``jobs_env`` channel to carry the width actually admitted
#: for this nested harness. A direct invocation has a full-strength width of up to eight;
#: a larger ambient value cannot make the test more expensive than that authored preference.
VALIDATION_JOBS_ENV = "AGENT_UTILS_VALIDATION_JOBS"
DEFAULT_VALIDATION_JOBS = 8


def _effective_validation_jobs() -> int:
    """Return this process's effective whole-core affinity/quota budget."""

    python_root = os.path.join(REPO_ROOT, "py")
    inserted = python_root not in sys.path
    if inserted:
        sys.path.insert(0, python_root)
    try:
        from dagrun.profile_enrich import container_core_budget

        return container_core_budget()
    finally:
        if inserted:
            sys.path.remove(python_root)


def _validation_jobs(
    env: Mapping[str, str] | None = None, *, effective_jobs: int | None = None
) -> int:
    """Return the portable CPU-bandwidth fixture width admitted by the outer runner."""

    environ = os.environ if env is None else env
    effective = _effective_validation_jobs() if effective_jobs is None else effective_jobs
    if effective < 1:
        raise ValueError(f"effective CPU budget must be positive, got {effective}")
    raw = environ.get(VALIDATION_JOBS_ENV)
    if raw is None:
        return min(effective, DEFAULT_VALIDATION_JOBS)
    if not raw or not raw.isascii() or not raw.isdecimal():
        raise ValueError(
            f"{VALIDATION_JOBS_ENV} must be a positive integer, got {raw!r}"
        )
    jobs = int(raw)
    if jobs < 1:
        raise ValueError(
            f"{VALIDATION_JOBS_ENV} must be a positive integer, got {raw!r}"
        )
    return min(jobs, effective, DEFAULT_VALIDATION_JOBS)


@dataclass(frozen=True)
class Invocation:
    name: str
    args: tuple[str, ...]


@dataclass(frozen=True)
class Fixture:
    """A DAG fixture plus the optional ``--max-mem`` budget to size it against."""

    name: str
    dag: dict[str, object]
    max_mem: str | None = None


@dataclass(frozen=True)
class Outcome:
    returncode: int
    stdout: str
    stderr: str
    elapsed_s: float = 0.0


@dataclass(frozen=True)
class CpuFootprintFacts:
    completed_steps: int
    workers_per_step: tuple[int, ...]
    max_live_steps: int
    max_live_workers: int


#: Why a check can be skipped instead of executed. A skip is its own result class: it is never a
#: pass, never increments ``Report.checks``, is listed by name in the summary, and makes the run
#: exit nonzero unless its kind was explicitly allowed for a partial run (``--allow-skip`` or
#: ``ALLOW_SKIP_ENV``).
SKIP_KINDS: dict[str, str] = {
    "boxing": "needs cgroup-v2 boxing: a working systemd --user scope or a delegated cgroup subtree",
    "delegated-live-scope": (
        "would create a live systemd scope from inside a parent-owned delegated cgroup"
    ),
    "hard-cpuset": "needs a HARD cpuset pin, which both engines refused on this host",
    "multi-cpu": (
        "needs at least two usable CPUs: two in the CPU affinity mask and a cgroup CPU quota of "
        "at least two cores"
    ),
    "eight-cpu": (
        "needs a core budget of at least eight CPUs, the tighter of the CPU affinity mask and "
        "any cgroup CPU quota"
    ),
    # herdr-run's end-to-end group: a C compiler for its shim, and no Herdr installed where
    # herdr-run would find it before the fake one.
    **HERDR_RUN_SKIP_KINDS,
}

#: Comma-separated skip kinds a lane declares acceptable, in addition to ``--allow-skip``. A lane
#: that sets it is partial by design and the harness says so in its final line.
ALLOW_SKIP_ENV = "AGENT_UTILS_CROSS_ALLOW_SKIP"


@dataclass(frozen=True)
class Skip:
    label: str
    kind: str
    reason: str


@dataclass
class Report:
    checks: int = 0
    failures: list[str] = field(default_factory=list)
    json_byte_identical: int = 0
    yaml_isomorphic: int = 0
    scalar_parity: int = 0
    skipped: list[Skip] = field(default_factory=list)

    def ok(self, _label: str) -> None:
        self.checks += 1

    def bad(self, label: str, detail: str) -> None:
        self.checks += 1
        self.failures.append(f"{label}: {detail}")

    def skip(self, label: str, kind: str, reason: str) -> None:
        """Record a check that did NOT run. It is not a pass and is not counted in ``checks``."""
        if kind not in SKIP_KINDS:
            raise ValueError(f"unknown skip kind {kind!r}; expected one of {sorted(SKIP_KINDS)}")
        self.skipped.append(Skip(label, kind, reason))


def skip_kind_counts(skips: Sequence[Skip]) -> str:
    """Each skip kind present with its count, sorted by kind: ``boxing 3, multi-cpu 2``.

    A partial verdict prints this so that a change in the mix of skips, such as a newly allowed
    kind, is visible in its final line and not only in the list above it.
    """

    counts: dict[str, int] = {}
    for skip in skips:
        counts[skip.kind] = counts.get(skip.kind, 0) + 1
    return ", ".join(f"{kind} {counts[kind]}" for kind in sorted(counts))


def report_skips(tool: str, rep: Report, allowed_skips: frozenset[str]) -> None:
    """List every skipped check by name, apart from and never mixed into the pass count."""

    if not rep.skipped:
        return
    print(
        f"cross[{tool}]: SKIPPED/UNVERIFIED {len(rep.skipped)} check(s), "
        "not counted as passes:"
    )
    for skip in rep.skipped:
        status = "allowed" if skip.kind in allowed_skips else "REQUIRED"
        print(f"  SKIPPED [{skip.kind}; {status}] {skip.label}: {skip.reason}")


#: A caller that needs each tool's final verdict as data, not only as a printed line, names a
#: directory here and every verdict leaves one JSON record in it. ``scripts/validate.py`` does
#: this, so that a validation node which passed with allowed skips shows up as PARTIAL in its
#: summary rather than as a plain pass.
COVERAGE_DIR_ENV = "AGENT_UTILS_CROSS_COVERAGE_DIR"

#: The verdicts a tool can end with. FAILED means at least one divergence; INCOMPLETE means no
#: divergence but a required check did not run. Both exit nonzero.
COVERAGE_VERDICTS = ("OK", "PARTIAL", "INCOMPLETE", "FAILED")


@dataclass(frozen=True)
class ToolCoverage:
    """One tool's final verdict: how many checks ran and which were skipped instead.

    ``checks`` is None for a tool whose differential keeps its own count and skips nothing
    through ``Report`` (herdr-agent, agentctl).
    """

    tool: str
    verdict: str
    checks: int | None
    skipped: tuple[Skip, ...] = ()


#: Every verdict this process has reached, in order.
_COVERAGE: list[ToolCoverage] = []


def record_coverage(
    tool: str, verdict: str, rep: Report | None, allowed_skips: frozenset[str]
) -> ToolCoverage:
    """Remember a tool's verdict and, when ``COVERAGE_DIR_ENV`` names a directory, write it out.

    A record that was asked for and cannot be written ends the run: a caller that summarises
    the records would otherwise report full coverage for a tool that skipped checks.
    """

    if verdict not in COVERAGE_VERDICTS:
        raise ValueError(f"unknown coverage verdict {verdict!r}")
    coverage = ToolCoverage(
        tool,
        verdict,
        None if rep is None else rep.checks,
        () if rep is None else tuple(rep.skipped),
    )
    _COVERAGE.append(coverage)
    directory = os.environ.get(COVERAGE_DIR_ENV)
    if not directory:
        return coverage
    record: dict[str, object] = {
        "schema": 1,
        "tool": tool,
        # The outer dagrun names the node that launched this process; absent at top level.
        "node": os.environ.get("DAGRUN_OUTER_RUN") or None,
        "verdict": verdict,
        "checks": coverage.checks,
        "skipped": [
            {
                "label": skip.label,
                "kind": skip.kind,
                "reason": skip.reason,
                "allowed": skip.kind in allowed_skips,
            }
            for skip in coverage.skipped
        ],
    }
    try:
        fd, temporary = tempfile.mkstemp(prefix=f".{tool}-", suffix=".tmp", dir=directory)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(record, handle, sort_keys=True)
            handle.write("\n")
        os.replace(
            temporary, os.path.join(directory, f"{tool}-{os.getpid()}-{len(_COVERAGE)}.json")
        )
    except OSError as exc:
        raise SystemExit(
            f"cross[{tool}]: cannot write the coverage record {COVERAGE_DIR_ENV}={directory} "
            f"asked for: {exc}"
        ) from exc
    return coverage


def divergence_verdict(
    tool: str, rep: Report, summary: str, allowed_skips: frozenset[str]
) -> int:
    """Print the final lines of a run that diverged, record it as FAILED, and return 1."""

    for failure in rep.failures:
        print(f"DIVERGENCE [{failure}]")
    report_skips(tool, rep, allowed_skips)
    print(f"cross[{tool}]: {len(rep.failures)} divergence(s) out of {summary}")
    record_coverage(tool, "FAILED", rep, allowed_skips)
    return 1


def coverage_aggregate(results: Sequence[tuple[int, ToolCoverage]]) -> tuple[str, int]:
    """Summarise a multi-tool run in one final line and return it with the run's exit status.

    Each tool has already printed its own verdict and listed its skips; this line exists so
    that the last thing a multi-tool run prints cannot read as a clean pass when any tool
    skipped checks or failed.
    """

    per_tool = [
        (coverage.tool, len(coverage.skipped)) for _, coverage in results if coverage.skipped
    ]
    total = sum(count for _, count in per_tool)
    skips = f"{total} skipped across tools"
    if per_tool:
        by_kind = skip_kind_counts([skip for _, coverage in results for skip in coverage.skipped])
        skips += (
            " ("
            + ", ".join(f"{tool} {count}" for tool, count in per_tool)
            + f"; by kind: {by_kind})"
        )
    failed = [(status, coverage) for status, coverage in results if status != 0]
    if failed:
        names = ", ".join(f"{coverage.tool} {coverage.verdict}" for _, coverage in failed)
        return (
            f"cross: FAILED - {len(failed)} of {len(results)} tool(s) did not pass: {names}; "
            f"{skips}",
            1,
        )
    if total:
        return (
            f"cross: PARTIAL - {skips}; no tool diverged, but the skipped checks are UNVERIFIED "
            "and listed above",
            0,
        )
    return f"cross: OK - all {len(results)} tools agree with no skipped checks", 0


def coverage_verdict(
    tool: str, rep: Report, summary: str, allowed_skips: frozenset[str]
) -> int:
    """Print the final line of a run without divergences and return its exit status.

    ``summary`` describes the checks that actually ran. Without an allowance every skip is a
    required check that did not run, so the run cannot claim full coverage and exits 1. Skips of
    allowed kinds make the run PARTIAL, which exits 0 and says so.
    """

    report_skips(tool, rep, allowed_skips)
    required = [skip for skip in rep.skipped if skip.kind not in allowed_skips]
    if required:
        kinds = ",".join(sorted({skip.kind for skip in required}))
        print(
            f"cross[{tool}]: INCOMPLETE - {summary}, but {len(required)} required check(s) "
            f"did not run: {', '.join(skip.label for skip in required)}. Full coverage cannot "
            f"pass with a skipped check; a lane that is partial by design must declare it with "
            f"--allow-skip {kinds} or {ALLOW_SKIP_ENV}={kinds}"
        )
        record_coverage(tool, "INCOMPLETE", rep, allowed_skips)
        return 1
    if rep.skipped:
        print(
            f"cross[{tool}]: PARTIAL - {summary}; {len(rep.skipped)} check(s) were skipped and "
            f"are UNVERIFIED (allowed skips by kind: {skip_kind_counts(rep.skipped)})"
        )
        record_coverage(tool, "PARTIAL", rep, allowed_skips)
        return 0
    print(f"cross[{tool}]: OK - {summary}")
    record_coverage(tool, "OK", rep, allowed_skips)
    return 0


def _deterministic_run_output(output: str) -> str:
    """Keep plan/run semantics while removing only volatile rounded terminal durations."""

    return _TERMINAL_STEP_DURATION_RE.sub(r"\1 (<elapsed>\2)", output)


# --------------------------------------------------------------------------- command wiring


def py_command() -> list[str]:
    return [sys.executable, "-m", "dagrun"]


_RUST_SNAPSHOTS: list[tempfile.TemporaryDirectory[str]] = []


def _sha256_file(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _read_provenance(path: str) -> tuple[str, str] | None:
    try:
        lines = Path(path).read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError):
        return None
    source_values = [line.removeprefix("source_fp=") for line in lines if line.startswith("source_fp=")]
    binary_values = [
        line.removeprefix("binary_sha256=")
        for line in lines
        if line.startswith("binary_sha256=")
    ]
    if len(source_values) != 1 or len(binary_values) != 1:
        return None
    if not re.fullmatch(r"[0-9a-f]{64}", source_values[0]) or not re.fullmatch(
        r"[0-9a-f]{64}", binary_values[0]
    ):
        return None
    return source_values[0], binary_values[0]


def rs_command(tool: str) -> list[str]:
    launcher = os.path.join(REPO_ROOT, "rs", "bin", tool)
    if not os.path.exists(launcher):
        raise FileNotFoundError(f"tracked Rust launcher for {tool!r} is missing: {launcher}")
    target_root = os.path.realpath(os.path.join(REPO_ROOT, "rs", "target"))
    lock_root = os.path.join(REPO_ROOT, "rs", ".agent-utils-locks")
    os.makedirs(lock_root, exist_ok=True)
    fingerprint = os.path.join(REPO_ROOT, "common", "bin", "rs-source-fingerprint")
    last_error = "launcher did not return an artifact"

    for _attempt in range(3):
        env = dict(os.environ)
        env["AGENT_UTILS_RS_ENSURE_ONLY"] = "1"
        ensured = subprocess.run(
            [launcher],
            cwd=REPO_ROOT,
            env=env,
            capture_output=True,
            text=True,
            check=False,
            timeout=300,
        )
        if ensured.returncode != 0:
            raise FileNotFoundError(
                f"Rust launcher for {tool!r} could not verify/build its cache: {ensured.stderr}"
            )
        candidate = ensured.stdout.strip()
        candidate_real = os.path.realpath(candidate)
        try:
            inside_target_root = os.path.commonpath([target_root, candidate_real]) == target_root
        except ValueError:
            inside_target_root = False
        expected_suffix = os.path.join("release", tool)
        if not inside_target_root or not candidate_real.endswith(os.sep + expected_suffix):
            raise FileNotFoundError(
                f"Rust launcher for {tool!r} returned an invalid executable path: {candidate!r}"
            )

        # The launcher releases its lock before returning. Reacquire the same outside-target lock,
        # then revalidate source + provenance around a private copy. A concurrent clean or raw Cargo
        # write either precedes this critical section or produces a hash mismatch and a retry.
        with open(os.path.join(lock_root, "cache.lock"), "a+b") as lock_file:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
            provenance = _read_provenance(candidate_real + ".provenance")
            source_before = subprocess.run(
                [fingerprint],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
                check=False,
                timeout=30,
            )
            if provenance is None or source_before.returncode != 0:
                last_error = "artifact or provenance disappeared before it could be copied"
                continue
            stamped_source, stamped_binary = provenance
            snapshot = tempfile.TemporaryDirectory(prefix=f"agent-utils-{tool}-")
            snapshot_path = os.path.join(snapshot.name, tool)
            try:
                shutil.copy2(candidate_real, snapshot_path)
                copied_binary = _sha256_file(snapshot_path)
            except OSError as exc:
                snapshot.cleanup()
                last_error = f"artifact copy failed: {exc}"
                continue
            source_after = subprocess.run(
                [fingerprint],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
                check=False,
                timeout=30,
            )
            if (
                source_after.returncode == 0
                and source_before.stdout.strip() == stamped_source
                and source_after.stdout.strip() == stamped_source
                and copied_binary == stamped_binary
                and os.access(snapshot_path, os.X_OK)
            ):
                _RUST_SNAPSHOTS.append(snapshot)
                # Hundreds of comparisons now reuse immutable private bytes, not Cargo's mutable
                # top-level target path, without repeatedly invoking the build frontend.
                return [snapshot_path]
            snapshot.cleanup()
            last_error = "source, artifact, or provenance changed while making the private copy"

    raise FileNotFoundError(f"Rust artifact for {tool!r} would not stabilize: {last_error}")


def _env(
    extra: Mapping[str, str] | None = None,
    *,
    inherit_runner_authority: bool = False,
) -> dict[str, str]:
    env = dict(os.environ)
    env["PYTHONPATH"] = os.path.join(REPO_ROOT, "py") + os.pathsep + env.get("PYTHONPATH", "")
    # Deterministic, color-free output regardless of the runner's TTY state.
    env["NO_COLOR"] = "1"
    # An outer validation DAG may deliberately delegate a bounded cgroup to THIS harness.  The
    # hundreds of synthetic child invocations are independent top-level inputs, however: they
    # must not mistake the harness's one-hop authority or 2.5-GiB physical ceiling for fixture
    # input.  A handful of tests intentionally exercise boxing; those retain authority explicitly
    # so they stay inside the parent-owned cgroup.  Policy knobs remain opt-in per fixture in both
    # modes, and ``extra`` is applied last for exactly that reason.
    if not inherit_runner_authority:
        for name in _RUNNER_AUTHORITY_ENV:
            env.pop(name, None)
    for name in _AMBIENT_CASE_CONTROL_ENV:
        env.pop(name, None)
    # Same reasoning, for the operator's build width. Both engines resolve intent from these two
    # variables in the OUTERMOST process, which here is the differential's own child — so a
    # developer who happens to export CARGO_BUILD_JOBS makes every case run under a width nobody
    # in this file chose, and turns the `operator-build-width:unstated` leg red for a reason that
    # has nothing to do with either engine. The cases that are ABOUT intent pass it in `extra`,
    # which is applied after these pops.
    env.pop("CARGO_BUILD_JOBS", None)
    env.pop("DAGRUN_OPERATOR_BUILD_JOBS", None)
    # Profile identity overrides belong only to the fixtures that set them explicitly. Ambient
    # values could make a writer emit one filename while a reader searches another and let an
    # interoperability check pass vacuously on two empty summaries.
    env.pop("DAGRUN_MACHINE_ID", None)
    # The retired spelling must not make a differential case accidentally pass.
    env.pop("SAFE_CI_DAG_RUNNER_JOBS_ENV", None)
    if extra:
        env.update(extra)
    return env


def _inside_delegated_harness() -> bool:
    """Return whether this differential itself is an outer dagrun delegated child."""

    return bool(
        os.environ.get("DAGRUN_DELEGATED_CGROUP")
        or os.environ.get("DAGRUN_DELEGATED_UNBOXED") == "1"
    )


def _parent_offers_only_unboxed_delegation() -> bool:
    """Return whether the outer scheduler explicitly has no cgroup subtree to delegate."""

    return (
        os.environ.get("DAGRUN_DELEGATED_UNBOXED") == "1"
        and bool(os.environ.get("DAGRUN_OUTER_RUN"))
        and not os.environ.get("DAGRUN_DELEGATED_CGROUP")
    )


def run(
    cmd: Sequence[str],
    args: Sequence[str],
    extra_env: Mapping[str, str] | None = None,
    *,
    timeout_s: float = 120.0,
    inherit_runner_authority: bool = False,
) -> Outcome:
    started = time.monotonic()
    try:
        proc = subprocess.run(
            [*cmd, *args],
            capture_output=True,
            text=True,
            env=_env(
                extra_env,
                inherit_runner_authority=inherit_runner_authority,
            ),
            start_new_session=True,
            check=False,
            timeout=timeout_s,
        )
    except subprocess.TimeoutExpired as exc:
        stdout = exc.stdout if isinstance(exc.stdout, str) else ""
        stderr = exc.stderr if isinstance(exc.stderr, str) else ""
        return Outcome(
            124,
            stdout,
            stderr + f"\nTIMEOUT after {timeout_s:g} seconds",
            time.monotonic() - started,
        )
    return Outcome(proc.returncode, proc.stdout, proc.stderr, time.monotonic() - started)


def run_until_stdout_contains(
    cmd: Sequence[str],
    args: Sequence[str],
    expected: str,
    *,
    timeout_s: float = 10.0,
) -> tuple[Outcome, bytes, bool]:
    """Run a command and retain the bytes observable when ``expected`` first arrives."""
    started = time.monotonic()
    proc = subprocess.Popen(
        [*cmd, *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=_env(),
        start_new_session=True,
    )
    assert proc.stdout is not None
    expected_bytes = expected.encode("utf-8")
    observed = bytearray()
    deadline = started + timeout_s
    while expected_bytes not in observed and time.monotonic() < deadline:
        readable, _, _ = select.select([proc.stdout], [], [], 0.05)
        if readable:
            chunk = os.read(proc.stdout.fileno(), 4096)
            if not chunk:
                break
            observed.extend(chunk)
        elif proc.poll() is not None:
            break
    observed_before_exit = expected_bytes in observed and proc.poll() is None
    # Give a wrongly streamed line immediately after the expected prefix a
    # chance to become observable while the final gate remains deliberately
    # asleep. Reading is nonblocking after the short sampling interval.
    if observed_before_exit:
        time.sleep(0.1)
        while select.select([proc.stdout], [], [], 0)[0]:
            chunk = os.read(proc.stdout.fileno(), 4096)
            if not chunk:
                break
            observed.extend(chunk)
    try:
        remaining_stdout, stderr = proc.communicate(timeout=max(0.1, deadline - time.monotonic()))
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        remaining_stdout, stderr = proc.communicate()
        stderr += f"\nTIMEOUT after {timeout_s:g} seconds".encode()
    stdout = bytes(observed) + remaining_stdout
    return (
        Outcome(
            proc.returncode if proc.returncode is not None else 124,
            stdout.decode("utf-8", errors="replace"),
            stderr.decode("utf-8", errors="replace"),
            time.monotonic() - started,
        ),
        bytes(observed),
        observed_before_exit,
    )


def run_with_closed_stdout(
    cmd: Sequence[str],
    args: Sequence[str],
    *,
    timeout_s: float = 10.0,
) -> Outcome:
    """Run with no stdout reader so the first flushed report gets EPIPE."""
    started = time.monotonic()
    read_fd, write_fd = os.pipe()
    os.close(read_fd)
    try:
        proc = subprocess.Popen(
            [*cmd, *args],
            stdout=write_fd,
            stderr=subprocess.PIPE,
            env=_env(),
            start_new_session=True,
        )
    finally:
        os.close(write_fd)
    assert proc.stderr is not None
    timed_out = False
    try:
        proc.wait(timeout=timeout_s)
    except subprocess.TimeoutExpired:
        timed_out = True
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()
    stderr = proc.stderr.read().decode("utf-8", errors="replace")
    if timed_out:
        stderr += f"\nTIMEOUT after {timeout_s:g} seconds"
    return Outcome(
        124 if timed_out else proc.returncode,
        "",
        stderr,
        time.monotonic() - started,
    )


# --------------------------------------------------------------------------- fixtures


def representative_fixtures() -> list[Fixture]:
    """Hand-written fixtures covering the load-bearing formatting + scheduling decisions."""
    fixtures: list[Fixture] = []

    fixtures.append(
        Fixture(
            "diamond",
            {
                "resource_caps": {"browser": 1, "net": 2},
                "mem_cap_factor": 1.25,
                "steps": [
                    {
                        "group": "build",
                        "job": "app",
                        "desc": "compile",
                        "cmd": "echo build",
                        # cpu_timeout is a per-step enforcement field; carrying it on a fixture step
                        # makes the json/yaml byte-identity round-trip cross-check its serialization
                        # across both engines (environment-independent), complementing the boxed
                        # behavioral smoke tests that only run where cgroups exist.
                        "cpu_timeout": 45,
                        "hint": {
                            "est_duration_s": 90,
                            "classification": "cpu-bound",
                            "rss_baseline_bytes": 1073741824,
                        },
                    },
                    {
                        "group": "test",
                        "job": "unit",
                        "desc": "unit tests",
                        "cmd": "echo test",
                        "deps": ["build.app"],
                    },
                    {
                        "group": "e2e",
                        "job": "smoke",
                        "desc": "browser smoke",
                        "cmd": "echo e2e",
                        "manifest": {"lane": "portable", "category": "applications"},
                        "result_manifests": [
                            {
                                "lane": "portable",
                                "category": "applications",
                                "mode": "verify",
                                "backend": "ptrace",
                            },
                            {
                                "lane": "portable",
                                "category": "c-programs",
                                "test": "c-programs/add-key-enosys",
                                "mode": "run",
                                "backend": "kvm",
                            },
                        ],
                        "integration_test_binaries": ["unit_alpha", "unit_beta"],
                        "deps": ["build.app"],
                        "hint": {"resources": {"browser": 1}, "classification": "latency-bound"},
                    },
                    {
                        "group": "e2e",
                        "job": "smoke2",
                        "desc": "browser smoke 2",
                        "cmd": "echo e2e2",
                        "result_manifests": [],
                        "deps": ["build.app"],
                        "hint": {"resources": {"browser": 1}},
                    },
                    {
                        "group": "e2e",
                        "job": "smoke3",
                        "desc": "singular fallback through null",
                        "cmd": "echo e2e3",
                        "manifest": {"lane": "portable", "category": "applications"},
                        "result_manifests": None,
                        "deps": ["build.app"],
                    },
                ],
            },
        )
    )

    fixtures.append(Fixture("empty", {"steps": []}))

    # Presence-sensitive per-node write domains: the no-protected-write node must retain an explicit
    # empty list, the writer must retain its structural guarantee, and both engines must execute
    # the valid graph without turning the domain into a scheduler mutex.
    fixtures.append(
        Fixture(
            "write_domains",
            {
                "write_domain_policy": {
                    "require_explicit": True,
                    "allowed_domains": ["shared-cargo-target", "isolated-target"],
                },
                "steps": [
                    {
                        "group": "g",
                        "job": "reader",
                        "cmd": "true",
                        "write_domains": [],
                    },
                    {
                        "group": "g",
                        "job": "barrier",
                        "cmd": "true",
                        "write_domains": ["shared-cargo-target"],
                        "write_domain_guarantee": "immutable-artifact-barrier",
                    },
                    {
                        "group": "g",
                        "job": "shielded",
                        "cmd": "true",
                        "deps": ["g.barrier"],
                        "write_domains": ["shared-cargo-target"],
                        "write_domain_guarantee": "artifact-barrier-dependent",
                    },
                    {
                        "group": "g",
                        "job": "isolated",
                        "cmd": "true",
                        "write_domains": ["isolated-target"],
                        "write_domain_guarantee": "explicitly-isolated",
                    },
                ],
            },
        )
    )

    # A failing step whose two dependents (transitively) must be skipped identically.
    fixtures.append(
        Fixture(
            "dep_failure_skip",
            {
                "steps": [
                    {"group": "g", "job": "a", "cmd": "false"},
                    {"group": "g", "job": "b", "cmd": "true", "deps": ["g.a"]},
                    {"group": "g", "job": "c", "cmd": "true", "deps": ["g.b"]},
                    {"group": "g", "job": "d", "cmd": "true"},
                ]
            },
        )
    )

    # JSON escaping + verbatim list rendering of special ASCII characters.
    fixtures.append(
        Fixture(
            "special_chars",
            {
                "description": 'top-level: "quotes", \\slash\\, unicode é☃, and a\nnewline',
                "steps": [
                    {
                        "group": "g",
                        "job": "quote",
                        "desc": 'has "quotes" and \\backslash\\ and\ttab',
                        # A multi-line description with quotes, backslashes, a tab, a control
                        # char, and unicode — proves the JSON string escaping is byte-identical
                        # across the two builds (FEATURE 1).
                        # Adversarial description covering hostile classes across the full escape
                        # surface: every C0 control (0x00 NUL .. 0x1f), DEL (0x7f), double
                        # backslashes, leading/trailing spaces, and the sneaky Unicode line/
                        # paragraph separators (U+2028/U+2029/U+0085) that some serializers escape
                        # and others pass raw. Both builds must agree byte-for-byte.
                        "description": (
                            "  leading+trailing spaces  \n"
                            'multi-line\nwith "quotes", \\backslash\\, double\\\\backslash, \ttab, '
                            "all-ctrl "
                            + "".join(chr(c) for c in range(0x00, 0x20))
                            + " del\x7f, unicode é☃\U0001F600, "
                            "line-seps \u2028\u2029\u0085 end"
                        ),
                        "cmd": "true",
                        "env": {"K2": "v2", "K1": "v1"},
                    }
                ],
            },
        )
    )

    # A memory-modeled DAG sized against a tight budget: the chosen --max-steps and footprint are
    # compared; the budget throttles below any plausible CPU count so the check is
    # CPU-count-independent.
    fixtures.append(
        Fixture(
            "sized",
            {
                "resource_caps": {"gpu": 1},
                "mem_cap_factor": 1.0,
                "outer_mem_safety_factor": 1.0,
                "mem_cap_floor_bytes": 0,
                "steps": [
                    {"group": "g", "job": "A", "cmd": "true", "hint": {"rss_baseline_bytes": 3221225472}},
                    {
                        "group": "g",
                        "job": "B",
                        "cmd": "true",
                        "deps": ["g.A"],
                        "hint": {"rss_baseline_bytes": 2147483648},
                    },
                    {
                        "group": "g",
                        "job": "C",
                        "cmd": "true",
                        "hint": {"resources": {"gpu": 1}, "rss_baseline_bytes": 4294967296},
                    },
                    {
                        "group": "g",
                        "job": "D",
                        "cmd": "true",
                        "hint": {"resources": {"gpu": 1}, "rss_baseline_bytes": 1073741824},
                    },
                ],
            },
            max_mem="6G",
        )
    )

    # jobs_flag: the inner-parallelism flag appended to a step's command when it declares
    # preferred_inner_jobs. Each fixture's command PASSES iff exactly the expected appended token(s)
    # arrive as "$*", so a serial run (-s1 with an ample -j budget) yields 1-passed in BOTH builds
    # only when Python and
    # Rust render + append the flag identically. The `json` check additionally pins schema parity
    # for the jobs_flag / default_jobs_flag fields.
    fixtures.append(
        Fixture(
            "jobs_flag_percent",  # "-j%d" -> "-j4" (no space)
            {
                "steps": [
                    {
                        "group": "g",
                        "job": "j",
                        "cmd": 'c() { [ "$*" = "-j4" ]; }; c',
                        "jobs_flag": "-j%d",
                        "hint": {"preferred_inner_jobs": 4},
                    }
                ]
            },
        )
    )
    fixtures.append(
        Fixture(
            "jobs_flag_equals",  # "--jobs=" -> "--jobs=8" (concatenated)
            {
                "steps": [
                    {
                        "group": "g",
                        "job": "j",
                        "cmd": 'c() { [ "$*" = "--jobs=8" ]; }; c',
                        "jobs_flag": "--jobs=",
                        "hint": {"preferred_inner_jobs": 8},
                    }
                ]
            },
        )
    )
    fixtures.append(
        Fixture(
            "jobs_flag_default_inherit",  # step inherits DAG default_jobs_flag "--num-threads" -> "--num-threads 3"
            {
                "default_jobs_flag": "--num-threads",
                "steps": [
                    {
                        "group": "g",
                        "job": "j",
                        "cmd": 'c() { [ "$*" = "--num-threads 3" ]; }; c',
                        "hint": {"preferred_inner_jobs": 3},
                    }
                ],
            },
        )
    )
    fixtures.append(
        Fixture(
            "jobs_env_schema",
            {
                "default_jobs_env": "CARGO_BUILD_JOBS",
                "steps": [
                    {
                        "group": "g",
                        "job": "inherit",
                        "cmd": "true",
                        "hint": {"preferred_inner_jobs": 1},
                    },
                    {
                        "group": "g",
                        "job": "override",
                        "cmd": "true",
                        "jobs_env": "CUSTOM_BUILD_JOBS",
                        "hint": {"preferred_inner_jobs": 1},
                    },
                    {
                        "group": "g",
                        "job": "opt-out",
                        "cmd": "true",
                        "jobs_env": "",
                    },
                ],
            },
        )
    )
    fixtures.append(
        Fixture(
            "cmdtype_simple_append",
            {
                "steps": [
                    {
                        "group": "g",
                        "job": "j",
                        "cmd": "sh -c 'test \"$#\" = 2 && test \"$1\" = --jobs && test \"$2\" = 3' c",
                        "cmdtype": "cargo-build",
                        "hint": {"preferred_inner_jobs": 3},
                    }
                ]
            },
        )
    )
    fixtures.append(
        Fixture(
            "cmdtype_compound_placement",
            {
                "steps": [
                    {
                        "group": "g",
                        "job": "j",
                        "cmd": 'c() { [ "$#" = 2 ] && [ "$1" = --jobs ] && [ "$2" = 3 ]; }; z() { [ "$#" = 0 ]; }; c $DAGRUN_EXTRA_ARGS && z',
                        "cmdtype": "cargo-build",
                        "hint": {"preferred_inner_jobs": 3},
                    }
                ]
            },
        )
    )
    fixtures.append(
        Fixture(
            "cmdtype_unknown_has_no_variable",
            {
                "steps": [
                    {
                        "group": "g",
                        "job": "j",
                        "cmd": 'test -z "${DAGRUN_EXTRA_ARGS+x}"',
                        "cmdtype": "unknown",
                        "jobs_flag": "",
                        "hint": {"preferred_inner_jobs": 3},
                    }
                ]
            },
        )
    )

    return fixtures


def _random_dag(rng: random.Random) -> tuple[dict[str, object], bool]:
    """Generate one acyclic DAG (deps only reference earlier steps).

    Returns the DAG document and whether any step carries an ``rss_baseline_bytes`` (so the
    caller can decide to attach a ``--max-mem`` budget for the sizing check).
    """
    groups = ["build", "test", "e2e", "lint"]
    classes = ["cpu-bound", "latency-bound", "light"]
    cmds = ["true", "true", "echo hi", "sleep 0.02", "echo x && sleep 0.02", "false"]
    caps: dict[str, int] = {"browser": rng.randint(1, 2), "net": rng.randint(1, 2)}

    n = rng.randint(1, 7)
    steps: list[object] = []
    tags: list[str] = []
    has_rss = False
    for i in range(n):
        group = rng.choice(groups)
        job = f"j{i}"
        tag = f"{group}.{job}"
        n_deps = rng.randint(0, min(2, len(tags)))
        deps = rng.sample(tags, n_deps) if n_deps else []
        hint: dict[str, object] = {}
        if rng.random() < 0.5:
            hint["classification"] = rng.choice(classes)
        if rng.random() < 0.4:
            # Only demand resources that exist in caps. An undeclared demand is refused before
            # any node starts (both engines), which is a correct outcome but not the one this
            # generator is here to compare.
            res_name = rng.choice(list(caps))
            hint["resources"] = {res_name: 1}
        if rng.random() < 0.3:
            hint["est_duration_s"] = round(rng.uniform(0.0, 120.0), 2)
        if rng.random() < 0.4:
            hint["rss_baseline_bytes"] = rng.randint(1, 8) * 1024**3
            has_rss = True
        step: dict[str, object] = {
            "group": group,
            "job": job,
            "desc": rng.choice(["", "a step", "does work"]),
            "cmd": rng.choice(cmds),
        }
        if deps:
            step["deps"] = deps
        if hint:
            step["hint"] = hint
        steps.append(step)
        tags.append(tag)

    dag: dict[str, object] = {"steps": steps, "resource_caps": caps}
    if rng.random() < 0.5:
        dag["mem_cap_factor"] = round(rng.uniform(1.0, 2.0), 2)
    if has_rss and rng.random() < 0.7:
        dag["mem_cap_floor_bytes"] = 0
        dag["outer_mem_safety_factor"] = 1.0
    return dag, has_rss


def randomized_fixtures(count: int, seed: int) -> list[Fixture]:
    rng = random.Random(seed)
    fixtures: list[Fixture] = []
    for i in range(count):
        dag, has_rss = _random_dag(rng)
        # Attach a sizing budget when the DAG participates in the memory model.
        fixtures.append(Fixture(f"rand{i:02d}", dag, max_mem="4G" if has_rss else None))
    return fixtures


def example_fixtures() -> list[Fixture]:
    """Every DAG shipped under ``examples/`` becomes a fixture, so the runnable newcomer examples
    are proven to render identically under both builds.

    These are checked STATIC-ONLY (``list``/``ascii``/``dot``/``json`` parity via
    :func:`compare_example_static`): the examples use real multi-second ``sleep`` commands to make
    parallelism visible, so folding them into the full run/timing battery would slow the harness for
    no added coverage — run/exit/counts/sizing parity is already exercised by the fast representative
    and randomized fixtures.
    """
    examples_dir = os.path.join(REPO_ROOT, "examples")
    if not os.path.isdir(examples_dir):
        return []
    fixtures: list[Fixture] = []
    for name in sorted(os.listdir(examples_dir)):
        if not name.endswith(".json"):
            continue
        with open(os.path.join(examples_dir, name), encoding="utf-8") as fh:
            loaded: object = json.load(fh)
        if not isinstance(loaded, dict):
            raise TypeError(f"examples/{name}: expected a JSON object at the top level")
        dag: dict[str, object] = {str(key): val for key, val in loaded.items()}
        fixtures.append(Fixture(f"example:{name[:-len('.json')]}", dag))
    return fixtures


def compare_example_static(py: list[str], rs: list[str], fx: Fixture, rep: Report) -> None:
    """Static-command parity for a shipped example DAG (see :func:`example_fixtures`)."""
    with tempfile.TemporaryDirectory() as tmp:
        dag_path = os.path.join(tmp, "dag.json")
        with open(dag_path, "w", encoding="utf-8") as fh:
            fh.write(json.dumps(fx.dag))
        _static_parity(py, rs, dag_path, fx.name, rep)


# --------------------------------------------------------------------------- YAML isomorphism


def yaml_fixture_paths() -> list[str]:
    """Every ``.yaml``/``.yml`` fixture: the shipped ``examples/`` plus the dedicated adversarial
    torture set under ``cross/yaml_fixtures/`` (block scalars, quoted Norway tokens, unicode)."""
    paths: list[str] = []
    for directory in (
        os.path.join(REPO_ROOT, "examples"),
        os.path.join(REPO_ROOT, "cross", "yaml_fixtures"),
    ):
        if not os.path.isdir(directory):
            continue
        for name in sorted(os.listdir(directory)):
            if name.endswith((".yaml", ".yml")):
                paths.append(os.path.join(directory, name))
    return paths


def compare_yaml_isomorphism(py: list[str], rs: list[str], rep: Report) -> None:
    """Assert YAML is ISOMORPHIC to JSON.

    For each YAML fixture: load it in BOTH builds, emit CANONICAL JSON, and assert the two are
    BYTE-IDENTICAL (so the two parsers agree — including on block scalars, the quoted Norway-problem
    tokens, and unicode). Additionally, when an ``examples/NAME.json`` pair exists, assert the
    ``.json`` and ``.yaml`` load to the SAME DAG in each language.
    """
    for path in yaml_fixture_paths():
        rel = os.path.relpath(path, REPO_ROOT)
        po = run(py, ("json", "--dag", path))
        ro = run(rs, ("json", "--dag", path))
        label = f"yaml-iso:{rel}"
        if po.returncode != 0 or ro.returncode != 0:
            rep.bad(
                label,
                f"YAML load failed: py rc={po.returncode} ({po.stderr!r}); "
                f"rs rc={ro.returncode} ({ro.stderr!r})",
            )
            continue
        if po.stdout != ro.stdout:
            rep.bad(
                label,
                f"YAML not isomorphic (py vs rs canonical JSON differ)\n"
                f"--- py ---\n{po.stdout}\n--- rs ---\n{ro.stdout}",
            )
            continue
        rep.ok(label)
        rep.yaml_isomorphic += 1

        json_pair = os.path.splitext(path)[0] + ".json"
        if not os.path.exists(json_pair):
            continue
        pj = run(py, ("json", "--dag", json_pair))
        rj = run(rs, ("json", "--dag", json_pair))
        plabel = f"yaml-pair:{rel}"
        if pj.returncode != 0 or rj.returncode != 0:
            rep.bad(plabel, f"JSON pair load failed: py rc={pj.returncode}; rs rc={rj.returncode}")
        elif pj.stdout != po.stdout:
            rep.bad(
                plabel,
                f"py: json/yaml pair loads differ\n--- json ---\n{pj.stdout}\n--- yaml ---\n{po.stdout}",
            )
        elif rj.stdout != ro.stdout:
            rep.bad(
                plabel,
                f"rs: json/yaml pair loads differ\n--- json ---\n{rj.stdout}\n--- yaml ---\n{ro.stdout}",
            )
        else:
            rep.ok(plabel)


def compare_include_composition(py: list[str], rs: list[str], rep: Report) -> None:
    """A shared multi-file fixture must flatten identically in both editions.

    It crosses JSON/YAML, nests namespaces, unions caps, rewrites identity fields, and exercises
    parent-owned ``after`` wiring at both nesting levels.
    """
    root = os.path.join(REPO_ROOT, "cross", "include_fixtures", "root.yaml")
    _static_parity(py, rs, root, "include-composition", rep)
    po = run(py, ("json", "--dag", root))
    ro = run(rs, ("json", "--dag", root))
    if po.returncode != 0 or ro.returncode != 0 or po.stdout != ro.stdout:
        rep.bad(
            "include-composition:semantic-shape",
            f"py={po.returncode}:{po.stderr!r}\nrs={ro.returncode}:{ro.stderr!r}",
        )
        return
    try:
        flattened: object = json.loads(po.stdout)
        if not isinstance(flattened, dict):
            raise TypeError("canonical document is not an object")
        steps = flattened.get("steps")
        if not isinstance(steps, list):
            raise TypeError("canonical document has no steps list")
        tags = [
            f"{step['group']}.{step['job']}"
            for step in steps
            if isinstance(step, dict)
            and isinstance(step.get("group"), str)
            and isinstance(step.get("job"), str)
        ]
        expected = [
            "component.leaf.build.app",
            "component.test.unit",
            "component.diagnose.unit",
            "component.prep.ready",
            "setup.ready",
            "publish.all",
        ]
        by_tag = {
            f"{step['group']}.{step['job']}": step
            for step in steps
            if isinstance(step, dict)
            and isinstance(step.get("group"), str)
            and isinstance(step.get("job"), str)
        }
        if (
            tags != expected
            or "include" in flattened
            or flattened.get("resource_caps") != {"browser": 1, "gpu": 2}
            or by_tag["component.leaf.build.app"].get("deps")
            != ["component.prep.ready"]
            or by_tag["component.leaf.build.app"].get("fail_fast_family")
            != "component.leaf.compile"
            or by_tag["component.diagnose.unit"].get("deps") != ["setup.ready"]
            or by_tag["component.prep.ready"].get("deps") != ["setup.ready"]
            # The parent names one exact internal node.  No synthetic include/completion node or
            # implicit whole-subgraph barrier may replace or augment this dependency.
            or by_tag["publish.all"].get("deps") != ["component.test.unit"]
        ):
            raise ValueError(f"unexpected flattened graph: {flattened!r}")
    except (json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
        rep.bad("include-composition:semantic-shape", str(error))
    else:
        rep.ok("include-composition:semantic-shape")

    plan_args = ("plan", "--dag", root, "--no-profile-feedback", "--format", "json")
    pp, rp = run(py, plan_args), run(rs, plan_args)
    if pp.returncode == rp.returncode == 0 and pp.stdout == rp.stdout:
        rep.ok("include-composition:plan")
    else:
        rep.bad("include-composition:plan", f"py={pp!r}\nrs={rp!r}")

    py_yaml, rs_yaml = run(py, ("yaml", "--dag", root)), run(rs, ("yaml", "--dag", root))
    if py_yaml.returncode == rs_yaml.returncode == 0:
        rep.ok("include-composition:yaml")
    else:
        rep.bad("include-composition:yaml", f"py={py_yaml!r}\nrs={rs_yaml!r}")

    run_args = (
        "run",
        "--dag",
        root,
        "--unsafe-no-cgroups",
        "--no-profile",
        "--no-profile-feedback",
        "-q",
    )
    pr, rr = run(py, run_args), run(rs, run_args)
    if pr.returncode == rr.returncode == 0 and _counts(pr.stderr) == _counts(rr.stderr):
        rep.ok("include-composition:run")
    else:
        rep.bad("include-composition:run", f"py={pr!r}\nrs={rr!r}")

    # An outer step may consume one exact nonterminal inside the fragment. Keep one active slot
    # and rank that consumer ahead of the remaining child terminal: the consumer creates the
    # marker which the terminal requires. This is deterministic dispatch-order evidence that an
    # include is not a synthetic completion barrier, without races or wall-clock assertions.
    exact_root = os.path.join(REPO_ROOT, "cross", "include_fixtures", "exact-edge-root.yaml")
    exact_args = (
        "run",
        "--dag",
        exact_root,
        "--unsafe-no-cgroups",
        "--no-profile",
        "--no-profile-feedback",
        "--max-steps",
        "1",
        "--planner",
        "greedy-lpt",
        "-q",
    )
    with tempfile.TemporaryDirectory(prefix="dagrun-include-exact-edge-") as tmp:
        markers = {
            "py": os.path.join(tmp, "python-marker"),
            "rs": os.path.join(tmp, "rust-marker"),
        }
        exact_py = run(py, exact_args, {"INCLUDE_EXACT_MARKER": markers["py"]})
        exact_rs = run(rs, exact_args, {"INCLUDE_EXACT_MARKER": markers["rs"]})
        if (
            exact_py.returncode == exact_rs.returncode == 0
            and all(os.path.isfile(marker) for marker in markers.values())
            and _counts(exact_py.stderr) == _counts(exact_rs.stderr)
        ):
            rep.ok("include-composition:exact-internal-edge-run")
        else:
            rep.bad(
                "include-composition:exact-internal-edge-run",
                f"py={exact_py!r}\nrs={exact_rs!r}\nmarkers={markers!r}",
            )
    invalid_dir = os.path.join(REPO_ROOT, "cross", "include_fixtures", "invalid")
    for name, phrase in (
        ("include-shape.json", "include: must be a list"),
        ("entry-number.json", "expected an object, got number"),
        ("unknown-entry-field.json", "unknown field(s) 'future'"),
        ("after-internal.json", "after must name outer steps"),
        ("cap-conflict.json", "resource cap 'a'=2 conflicts"),
        ("after-empty.json", "after references missing outer step 'missing.step'"),
        ("cycle-a.json", "include cycle:"),
    ):
        path = os.path.join(invalid_dir, name)
        pi, ri = run(py, ("json", "--dag", path)), run(rs, ("json", "--dag", path))
        label = f"include-composition:refusal:{name}"
        if (
            pi.returncode == ri.returncode == 2
            and phrase in pi.stderr
            and pi.stderr == ri.stderr
        ):
            rep.ok(label)
        else:
            rep.bad(label, f"py={pi!r}\nrs={ri!r}")


def compare_delegated_children_schema(py: list[str], rs: list[str], rep: Report) -> None:
    """The containment-preserving nested-run opt-in is one typed, cross-edition field."""
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "delegated.json")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(
                '{"steps":['
                '{"group":"g","job":"delegating","cmd":"true",'
                '"delegated_children":true},'
                '{"group":"g","job":"ordinary","cmd":"true"}]}'
            )
        _static_parity(py, rs, path, "delegated-children-schema", rep)
        po, ro = run(py, ("json", "--dag", path)), run(rs, ("json", "--dag", path))
        if (
            po.returncode == ro.returncode == 0
            and po.stdout == ro.stdout
            and po.stdout.count('"delegated_children"') == 1
        ):
            rep.ok("delegated-children-schema:explicit-only")
        else:
            rep.bad(
                "delegated-children-schema:explicit-only",
                f"py={po!r}\nrs={ro!r}",
            )

        with open(path, "w", encoding="utf-8") as fh:
            fh.write(
                '{"steps":[{"group":"g","job":"bad","cmd":"true",'
                '"delegated_children":"yes"}]}'
            )
        po, ro = run(py, ("json", "--dag", path)), run(rs, ("json", "--dag", path))
        if po.returncode == ro.returncode == 2 and po.stderr == ro.stderr:
            rep.ok("delegated-children-schema:wrong-type-refusal")
        else:
            rep.bad(
                "delegated-children-schema:wrong-type-refusal",
                f"py={po!r}\nrs={ro!r}",
            )


# --------------------------------------------------------------------------- scalar parity


def scalar_parity_cases() -> list[tuple[str, str, str]]:
    """``(name, extension, document)`` triples pinning cross-language AGREEMENT on adversarial
    scalar resolution, non-finite / overflow floats, and out-of-range integers.

    Unlike :func:`compare_yaml_isomorphism` (which only covers documents BOTH builds accept), each
    case here asserts the two builds return the SAME exit code — so "both correctly REJECT" is a
    pass — and, when both accept, byte-identical canonical JSON. This is the regression net for the
    PyYAML-1.1-vs-serde_norway-1.2 divergences (the "Norway problem", octal/underscore/sexagesimal/
    timestamp resolution), Python-vs-serde_json non-finite handling, and i64-range integer bounds.
    """
    # A single valid step, appended after whichever adversarial top-level/step field is under test.
    step = "steps:\n  - {group: g, job: j, cmd: make}\n"
    cases: list[tuple[str, str, str]] = []

    # Norway problem in a STRING field (top-level `description`): plain yes/no/on/off/y/n must stay
    # strings (accept); true/false resolve to bools (reject as non-string) — the SAME on both sides.
    for tok in ("no", "yes", "on", "off", "y", "n", "Yes", "No"):
        cases.append((f"norway_str_{tok}", "yaml", f"description: {tok}\n{step}"))
    for tok in ("true", "false", "True", "FALSE"):
        cases.append((f"bool_in_str_{tok}", "yaml", f"description: {tok}\n{step}"))

    # Norway problem in a BOOL field (`networkonly`): only true/false are bools; yes/no/on/off are
    # strings and must be rejected by the boolean field — identically in both builds.
    for tok in ("true", "false"):
        cases.append((f"bool_field_ok_{tok}", "yaml", f"steps:\n  - {{group: g, job: j, cmd: make, networkonly: {tok}}}\n"))
    for tok in ("no", "yes", "on", "off"):
        cases.append((f"bool_field_reject_{tok}", "yaml", f"steps:\n  - {{group: g, job: j, cmd: make, networkonly: {tok}}}\n"))

    # Integer resolution in an INT field (`default_step_timeout`).
    for name, tok in (
        ("dec", "42"), ("plus", "+5"), ("minus", "-5"), ("zero", "0"),
        ("octal_prefixed", "0o17"), ("hex", "0x10"), ("binary", "0b101"),
        ("leading_zero", "0755"), ("leading_zero2", "010"),
        ("underscore", "1_000"), ("sexagesimal", "1:20"), ("floatish", "1e3"),
        ("bignum", "99999999999999999999999999"),
        ("just_over_i64", "9223372036854775808"),
    ):
        cases.append((f"int_{name}", "yaml", f"default_step_timeout: {tok}\n{step}"))

    # Float resolution in a FLOAT field (`mem_cap_factor`).
    for name, tok in (
        ("sci", "1e3"), ("dot_frac", ".5"), ("trailing_dot", "1."),
        ("overflow", "1e400"), ("neg_overflow", "-1e400"),
        ("inf", ".inf"), ("nan", ".nan"), ("leading_zero", "0755"),
    ):
        cases.append((f"float_{name}", "yaml", f"mem_cap_factor: {tok}\n{step}"))

    # Non-finite in a NULLABLE float field: serde maps `.inf` to null, so both read null (accept).
    cases.append(("nullable_inf", "yaml", "steps:\n  - {group: g, job: j, cmd: make, hint: {measured_effective_cores: .inf}}\n"))

    # String field fed YAML null tokens: `description` has default "" but an explicit null value is
    # not a string and must be rejected by BOTH builds.
    for name, tok in (("null_word", "null"), ("tilde", "~")):
        cases.append((f"str_{name}", "yaml", f"description: {tok}\n{step}"))

    # Timestamp / time-like tokens must stay plain strings in a string field (accept on both).
    for name, tok in (("date", "2024-01-01"), ("time", "12:34:56")):
        cases.append((f"str_{name}", "yaml", f"description: {tok}\n{step}"))

    # Malformed YAML must be a load error (exit 2), not an uncaught exception, on both builds.
    cases.append(("malformed_yaml", "yaml", "steps: [ this is : broken : yaml"))

    # JSON non-finite literals: Python's json accepts them by default, serde_json rejects — both
    # must reject here.
    for name, tok in (("infinity", "Infinity"), ("neg_infinity", "-Infinity"), ("nan", "NaN")):
        cases.append((f"json_{name}", "json", f'{{"mem_cap_factor": {tok}, "steps": []}}'))
    # JSON overflow-to-infinity and out-of-i64-range integer: both reject.
    cases.append(("json_overflow_float", "json", '{"mem_cap_factor": 1e400, "steps": []}'))
    cases.append(("json_bignum", "json", '{"mem_cap_floor_bytes": 99999999999999999999999999, "steps": []}'))

    # Write-domain policy must refuse identically before execution: omitted is not explicit
    # no-protected-write declaration, and neither unknown nor duplicate domains can silently enter
    # the vocabulary.
    cases.append((
        "write_domain_missing",
        "json",
        '{"steps":[{"group":"g","job":"j","cmd":"true"}],'
        '"write_domain_policy":{"require_explicit":true,"allowed_domains":[]}}',
    ))
    cases.append((
        "write_domain_unknown",
        "json",
        '{"steps":[{"group":"g","job":"j","cmd":"true",'
        '"write_domains":["typo"],"write_domain_guarantee":"artifact-producer"}],'
        '"write_domain_policy":{"require_explicit":true,'
        '"allowed_domains":["shared-cargo-target"]}}',
    ))
    cases.append((
        "write_domain_duplicate",
        "json",
        '{"steps":[{"group":"g","job":"j","cmd":"true",'
        '"write_domains":["shared-cargo-target","shared-cargo-target"]}],'
        '"write_domain_policy":{"require_explicit":true,'
        '"allowed_domains":["shared-cargo-target"]}}',
    ))

    # Scientific-notation float VALUES must serialize byte-identically (the json_float parity fix).
    for name, tok in (
        ("e20", "1e20"), ("e_minus7", "1e-7"), ("e16", "1e16"), ("e100", "1e100"),
        ("big", "1.2345678901234568e17"), ("e_minus5", "1e-5"), ("fixed_e15", "1e15"),
    ):
        cases.append((f"sci_{name}", "json", f'{{"mem_cap_factor": {tok}, "steps": []}}'))

    # Duplicate mapping keys: both builds accept last-wins, on both YAML and JSON, byte-identically.
    cases.append(("dup_key_yaml", "yaml", "steps:\n  - {group: g, job: j, cmd: first, cmd: second}\n"))
    cases.append(("dup_key_json", "json", '{"steps": [{"group": "g", "job": "j", "cmd": "first", "cmd": "second"}]}'))

    return cases


def compare_scalar_parity(py: list[str], rs: list[str], rep: Report) -> None:
    """Run every :func:`scalar_parity_cases` document through both builds and assert exit-code parity
    (so "both reject" passes) plus byte-identical canonical JSON whenever both accept."""
    with tempfile.TemporaryDirectory() as tmp:
        for i, (name, ext, doc) in enumerate(scalar_parity_cases()):
            path = os.path.join(tmp, f"case{i}.{ext}")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(doc)
            po = run(py, ("json", "--dag", path))
            ro = run(rs, ("json", "--dag", path))
            label = f"scalar-parity:{name}"
            if po.returncode != ro.returncode:
                rep.bad(
                    label,
                    f"exit py={po.returncode} rs={ro.returncode} "
                    f"(py stderr={po.stderr!r}; rs stderr={ro.stderr!r})",
                )
            elif po.returncode == 0 and po.stdout != ro.stdout:
                rep.bad(
                    label,
                    f"both accepted but json differs\n--- py ---\n{po.stdout}\n--- rs ---\n{ro.stdout}",
                )
            else:
                rep.ok(label)
                rep.scalar_parity += 1


# --------------------------------------------------------------------------- comparisons


def _counts(stderr: str) -> tuple[int, int, int, int, int, int] | None:
    m = _COUNTS_RE.search(stderr)
    if not m:
        return None
    return (
        int(m.group(1)),
        int(m.group(2)),
        int(m.group(3)),
        int(m.group(4)),
        int(m.group(5)),
        int(m.group(6)),
    )


def _sizing_details(stderr: str) -> tuple[int, int, int, int, int] | None:
    """Return ``(memory_ceiling, footprint, budget, base_ceiling, final_ceiling)``."""
    m = _SIZING_RE.search(stderr)
    if not m:
        return None
    memory_steps = int(m.group(1))
    footprint = int(m.group(2))
    budget = int(m.group(3))
    base = int(m.group(4))
    selected = int(m.group(5))
    if selected != min(base, memory_steps):
        return None
    return memory_steps, footprint, budget, base, selected


def _sizing(stderr: str) -> tuple[int, int, int] | None:
    details = _sizing_details(stderr)
    if details is None:
        return None
    _memory_steps, footprint, budget, _base, selected = details
    return selected, footprint, budget


def _sizing_refusal(stderr: str) -> tuple[int, int] | None:
    """Return ``(minimum_footprint, budget)`` from a fail-closed max-memory refusal."""
    match = _SIZING_REFUSAL_RE.search(stderr)
    if match is None:
        return None
    return int(match.group(1)), int(match.group(2))


def _static_parity(py: list[str], rs: list[str], dag_path: str, name: str, rep: Report) -> None:
    """Assert the static (non-running) commands agree for one DAG file: ``list``/``ascii``/``dot``
    AND ``json`` stdout are all BYTE-IDENTICAL.

    Both builds now emit ``ensure_ascii=False`` canonical JSON, so the ``json`` output must match
    byte-for-byte for every input, including hairy ``description`` fields (multi-line, quotes,
    backslashes, control chars, unicode). A parse-only difference is still a failure — the note in
    the failure message localizes whether it is a formatting-only or a semantic divergence.

    Shared by the full-battery fixtures (:func:`compare_fixture`) and the shipped-example fixtures
    (:func:`compare_example_static`), so the two paths cannot drift apart.
    """
    # list / ascii / dot, and the group-level ascii / dot views: byte-identical stdout.
    for mode in ("list", "ascii", "dot", "ascii --group-by group", "dot --group-by group"):
        po = run(py, (*mode.split(), "--dag", dag_path))
        ro = run(rs, (*mode.split(), "--dag", dag_path))
        label = f"{name}/{mode}"
        if po.returncode != ro.returncode:
            rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}")
        elif po.stdout != ro.stdout:
            rep.bad(label, f"stdout differs\n--- py ---\n{po.stdout}\n--- rs ---\n{ro.stdout}")
        else:
            rep.ok(label)

    # json: BYTE-identical stdout.
    po = run(py, ("json", "--dag", dag_path))
    ro = run(rs, ("json", "--dag", dag_path))
    label = f"{name}/json"
    if po.returncode != ro.returncode:
        rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}")
        return
    if po.stdout != ro.stdout:
        try:
            note = "parse-equal" if json.loads(po.stdout) == json.loads(ro.stdout) else "parse-DIFFER"
        except json.JSONDecodeError as exc:
            note = f"unparseable: {exc}"
        rep.bad(
            label,
            f"json stdout not byte-identical ({note})\n--- py ---\n{po.stdout}\n--- rs ---\n{ro.stdout}",
        )
        return
    rep.ok(label)
    rep.json_byte_identical += 1


def _first_selection(fx: Fixture) -> tuple[str, int] | None:
    """The first step's tag and dependency-inclusive node count, or ``None`` for an odd DAG.

    Used to exercise ``--selected`` parity on a concrete, present tag."""
    steps = fx.dag.get("steps")
    if not isinstance(steps, list) or not steps:
        return None
    first = steps[0]
    if not isinstance(first, dict):
        return None
    group, job = first.get("group"), first.get("job")
    if not isinstance(group, str) or not isinstance(job, str):
        return None
    tag = f"{group}.{job}"
    direct: dict[str, tuple[str, ...]] = {}
    for step in steps:
        if not isinstance(step, dict):
            return None
        step_group, step_job, deps = step.get("group"), step.get("job"), step.get("deps", [])
        if (
            not isinstance(step_group, str)
            or not isinstance(step_job, str)
            or not isinstance(deps, list)
            or any(not isinstance(dep, str) for dep in deps)
        ):
            return None
        direct[f"{step_group}.{step_job}"] = tuple(deps)
    selected = {tag}
    pending = [tag]
    while pending:
        current = pending.pop()
        for dep in direct.get(current, ()):
            if dep not in direct:
                return None
            if dep not in selected:
                selected.add(dep)
                pending.append(dep)
    return tag, len(selected)


def compare_selected_behavior(py: list[str], rs: list[str], rep: Report) -> None:
    """Selected-step dependency handling and usage errors must agree in both builds."""
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "dag.json")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(
                '{"steps": ['
                '{"group": "a", "job": "root", "cmd": "true"},'
                '{"group": "b", "job": "middle", "cmd": "true", "deps": ["a.root"]},'
                '{"group": "c", "job": "leaf", "cmd": "true", "deps": ["b.middle"]}'
                "]}"
            )

        cases = (
            ("selected-dependency-ancestry", ("--selected", "c.leaf"), 0, 3),
            (
                "selected-ignore-dependencies",
                ("--selected", "c.leaf", "--ignore-selected-deps"),
                0,
                1,
            ),
            ("selected-ignore-requires-selection", ("--ignore-selected-deps",), 2, None),
            ("selected-unknown-tag", ("--selected", "no.pe"), 2, None),
            ("selected-retired-only-flag", ("--only", "c.leaf"), 2, None),
            (
                "selected-ignore-boolean-value",
                ("--selected", "c.leaf", "--ignore-selected-deps=true"),
                2,
                None,
            ),
        )
        for label, extra, expected_exit, expected_nodes in cases:
            args = ("run", "--dag", path, "-q", *extra, NOPROF, NOFB, UNBOXED)
            po = run(py, args)
            ro = run(rs, args)
            pc, rc = _counts(po.stderr), _counts(ro.stderr)
            if po.returncode != ro.returncode:
                rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}")
            elif po.returncode != expected_exit:
                rep.bad(label, f"expected exit {expected_exit}; got {po.returncode}")
            elif expected_nodes is None:
                rep.ok(label)
            elif pc is None or rc is None:
                rep.bad(label, f"missing summary counts py={po.stderr!r} rs={ro.stderr!r}")
            elif pc != rc:
                rep.bad(label, f"counts py={pc} rs={rc}")
            elif sum(pc) != expected_nodes:
                rep.bad(label, f"expected {expected_nodes} accounted nodes; got counts {pc}")
            else:
                rep.ok(label)


#: Top-level DAG-document keys that name a real ``DagConfig`` field the format cannot carry, and
#: that BOTH editions must therefore refuse by name rather than silently default.
#:
#: Written out here, in the one mechanism that can see both builds at once.  Each edition's own
#: suite pins its own copy, but "both editions refuse the same keys byte for byte" — which both
#: loaders' doc comments claim — is a statement about the PAIR, and nothing was checking it: a
#: key dropped from one edition alone left every test in both suites green and made a document
#: that loads on one build and is rejected on the other.
UNCARRIED_CONFIG_KEYS = (
    "default_step_mem_cap_bytes",
    "default_step_cpu_count",
    "default_step_cpu_timeout",
    "cpu_timeout_multiplier",
    "cpu_timeout_platform",
    "known_failures",
)


def compare_uncarried_config_keys(py: list[str], rs: list[str], rep: Report) -> None:
    """Both builds must refuse the same uncarried top-level keys, with the same message.

    ``known_failures`` is in the set although only the Python edition has such a field: the key
    set is a portability contract, and a document is not more portable for being accepted by one
    build and rejected by the other.
    """
    with tempfile.TemporaryDirectory() as tmp:
        for key in UNCARRIED_CONFIG_KEYS:
            path = os.path.join(tmp, f"{key}.json")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write('{"%s": 5, "steps": []}' % key)
            po = run(py, ("plan", "--dag", path))
            ro = run(rs, ("plan", "--dag", path))
            label = f"uncarried-key:{key}"
            if po.returncode != ro.returncode:
                rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}")
            elif po.returncode != 2:
                rep.bad(
                    label,
                    f"a key naming an uncarried DagConfig field must be REFUSED (exit 2), not "
                    f"silently defaulted; both builds exited {po.returncode}",
                )
            elif po.stderr != ro.stderr:
                rep.bad(label, f"stderr py={po.stderr!r} rs={ro.stderr!r}")
            elif key not in po.stderr or "SILENTLY replaced by a default" not in po.stderr:
                rep.bad(label, f"the refusal must name the key: {po.stderr!r}")
            else:
                rep.ok(label)
        # The other side of the contract, so "refuse everything" cannot pass the cases above: a
        # key naming nothing at all cannot masquerade as a setting that took effect, so both
        # builds still accept it.
        path = os.path.join(tmp, "unknown.json")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write('{"future_thing": 5, "steps": []}')
        po = run(py, ("plan", "--dag", path))
        ro = run(rs, ("plan", "--dag", path))
        label = "uncarried-key:unimplemented-key-still-tolerated"
        if po.returncode != ro.returncode:
            rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}")
        elif po.returncode != 0:
            rep.bad(label, f"an unimplemented key must stay tolerated; got {po.returncode}")
        else:
            rep.ok(label)


#: Every graph the loader must REFUSE, as ``(label, document, phrase the refusal must contain)``.
#:
#: All five were accepted by both builds until the loader learned to check the graph, and four of
#: them then produced a wrong run rather than an error: a field that did nothing, a step that
#: never ran while the summary counted it as passed, a starve found only after unrelated work had
#: completed, and a crash (``RecursionError`` in Python, a stack-overflow ABORT WITH CORE DUMP in
#: Rust). Pinned here so the two editions cannot drift back apart one refusal at a time.
LOADER_REFUSALS: tuple[tuple[str, str, str], ...] = (
    (
        "different-document-type",
        '{"schema":2,"bucket":"example","test":[]}',
        "expected a dagrun DAG document with a top-level 'steps' list; found no 'steps' key "
        "(top-level keys: 'bucket', 'schema', 'test')",
    ),
    (
        "unknown-step-field",
        '{"steps":[{"group":"a","job":"one","cmd":"true","bogus_field":42}]}',
        "steps[0] (a.one): unknown field(s) 'bogus_field'",
    ),
    (
        "unknown-cmdtype",
        '{"steps":[{"group":"a","job":"one","cmd":"true","cmdtype":"cargo"}]}',
        "valid values: unknown, make, cargo-build, cargo-test, cargo-nextest, "
        "generic-dash-j-command, generic-with-flag",
    ),
    (
        "quoted-multi-word-extra-args",
        '{"steps":[{"group":"a","job":"one",'
        '"cmd":"cargo build \\"$DAGRUN_EXTRA_ARGS\\"","cmdtype":"cargo-build",'
        '"hint":{"preferred_inner_jobs":3}}]}',
        "DAGRUN_EXTRA_ARGS must be unquoted",
    ),
    (
        "compound-cmdtype-without-placement",
        '{"steps":[{"group":"a","job":"one","cmd":"prepare && cargo build",'
        '"cmdtype":"cargo-build","hint":{"preferred_inner_jobs":3}}]}',
        "compound cmd with cmdtype cargo-build must place unquoted",
    ),
    (
        "unknown-hint-field",
        '{"steps":[{"group":"a","job":"one","cmd":"true","hint":{"est_duration":9}}]}',
        "steps[0].hint: unknown field(s) 'est_duration'",
    ),
    (
        "incomplete-manifest-selection",
        '{"steps":[{"group":"a","job":"one","cmd":"true",'
        '"manifest":{"lane":"portable"}}]}',
        "steps[0].manifest: field 'category' must be a string",
    ),
    (
        "unknown-manifest-field",
        '{"steps":[{"group":"a","job":"one","cmd":"true",'
        '"manifest":{"lane":"portable","category":"applications","future":1}}]}',
        "steps[0].manifest: unknown field(s) 'future'",
    ),
    (
        "result-manifests-not-a-list",
        '{"steps":[{"group":"a","job":"one","cmd":"true",'
        '"result_manifests":{"lane":"portable","category":"applications"}}]}',
        "steps[0].result_manifests: must be a list of result declarations or null",
    ),
    (
        "incomplete-result-manifest-selector",
        '{"steps":[{"group":"a","job":"one","cmd":"true",'
        '"result_manifests":[{"lane":"portable"}]}]}',
        "steps[0].result_manifests[0]: field 'category' must be a string",
    ),
    (
        "unknown-result-manifest-field",
        '{"steps":[{"group":"a","job":"one","cmd":"true",'
        '"result_manifests":[{"lane":"portable","category":"applications",'
        '"future":1}]}]}',
        "steps[0].result_manifests[0]: unknown field(s) 'future'",
    ),
    (
        "duplicate-result-manifest-selector",
        '{"steps":[{"group":"a","job":"one","cmd":"true",'
        '"result_manifests":[{"lane":"portable","category":"applications"},'
        '{"lane":"portable","category":"applications"}]}]}',
        "steps[0].result_manifests: duplicate selector at index 1",
    ),
    (
        "unknown-result-manifest-kind",
        '{"steps":[{"group":"test","job":"counts","cmd":"true",'
        '"result_manifests":[{"kind":"future","schema":3,'
        '"path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"test.counts"}]}]}',
        "steps[0].result_manifests[0].kind: unknown result-manifest kind 'future'",
    ),
    (
        "wrong-structured-result-schema",
        '{"steps":[{"group":"test","job":"counts","cmd":"true",'
        '"result_manifests":[{"kind":"structured-test-results","schema":5,'
        '"path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"test.counts"}]}]}',
        "steps[0].result_manifests[0].schema: structured test results require default schema 2, "
        "classified schema 3 or diagnostic schema 4, got 5",
    ),
    (
        "wrong-structured-result-path",
        '{"steps":[{"group":"test","job":"counts","cmd":"true",'
        '"result_manifests":[{"kind":"structured-test-results","schema":2,'
        '"path_env":"OTHER","owner":"test.counts"}]}]}',
        "steps[0].result_manifests[0].path_env: structured test results require "
        "'DAGRUN_TEST_COUNTS_PATH', got 'OTHER'",
    ),
    (
        "wrong-structured-result-owner",
        '{"steps":[{"group":"test","job":"counts","cmd":"true",'
        '"result_manifests":[{"kind":"structured-test-results","schema":2,'
        '"path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"other.step"}]}]}',
        "step test.counts: structured test-result owner 'other.step' must equal the containing step tag",
    ),
    (
        "duplicate-structured-result-declarations",
        '{"steps":[{"group":"test","job":"counts","cmd":"true",'
        '"result_manifests":[{"kind":"structured-test-results","schema":2,'
        '"path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"test.counts"},'
        '{"kind":"structured-test-results","schema":2,'
        '"path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"test.counts"}]}]}',
        "steps[0].result_manifests: duplicate selector at index 1",
    ),
    (
        "empty-integration-test-binary",
        '{"steps":[{"group":"a","job":"one","cmd":"true",'
        '"integration_test_binaries":["unit_alpha",""]}]}',
        "field 'integration_test_binaries' must not contain empty names",
    ),
    (
        "duplicate-integration-test-binary",
        '{"steps":[{"group":"a","job":"one","cmd":"true",'
        '"integration_test_binaries":["unit_alpha","unit_alpha"]}]}',
        "field 'integration_test_binaries' contains duplicate name 'unit_alpha'",
    ),
    (
        "unknown-write-domain-policy-field",
        '{"steps":[],"write_domain_policy":{"require_explicits":true}}',
        "write_domain_policy: unknown field(s) 'require_explicits'",
    ),
    (
        "duplicate-step-tag",
        '{"steps":[{"group":"a","job":"one","cmd":"echo FIRST"},'
        '{"group":"a","job":"one","cmd":"echo SECOND"}]}',
        "duplicate step tag 'a.one': declared 2 times",
    ),
    (
        "missing-dependency",
        '{"steps":[{"group":"z","job":"zero","cmd":"true"},'
        '{"group":"a","job":"one","cmd":"true","deps":["b.missing"]}]}',
        "step a.one: depends on 'b.missing', which no step declares",
    ),
    (
        "dependency-cycle",
        '{"steps":[{"group":"a","job":"one","cmd":"true","deps":["b.two"]},'
        '{"group":"b","job":"two","cmd":"true","deps":["a.one"]}]}',
        "dependency cycle: a.one -> b.two -> a.one",
    ),
    (
        "self-dependency",
        '{"steps":[{"group":"a","job":"one","cmd":"true","deps":["a.one"]}]}',
        "dependency cycle: a.one -> a.one",
    ),
    (
        "demand-above-positive-cap",
        '{"resource_caps":{"browser":1},"steps":[{"group":"a","job":"one","cmd":"true",'
        '"hint":{"resources":{"browser":2}}}]}',
        "step a.one: demands browser=2 but resource_caps declares browser=1",
    ),
    (
        "several-faults-at-once",
        '{"resource_caps":{"browser":1},'
        '"steps":[{"group":"a","job":"one","cmd":"true","deps":["nope.gone"]},'
        '{"group":"b","job":"two","cmd":"true","deps":["c.three"]},'
        '{"group":"c","job":"three","cmd":"true","deps":["b.two"]},'
        '{"group":"d","job":"four","cmd":"true","hint":{"resources":{"browser":5}}}]}',
        "3 graph error(s)",
    ),
)

#: Graphs that must still LOAD, so "refuse everything" cannot pass the table above. Each one is
#: the near-miss of a refusal: the boundary is the part that is easy to get wrong in only one
#: edition.
LOADER_ACCEPTANCES: tuple[tuple[str, str], ...] = (
    (
        "classified-result-schema-is-explicit",
        '{"steps":[{"group":"test","job":"counts","cmd":"true",'
        '"result_manifests":[{"kind":"structured-test-results","schema":3,'
        '"path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"test.counts"}]}]}',
    ),
    (
        "diagnostic-result-schema-is-explicit",
        '{"steps":[{"group":"test","job":"counts","cmd":"true",'
        '"result_manifests":[{"kind":"structured-test-results","schema":4,'
        '"path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"test.counts"}]}]}',
    ),
    (
        "every-declared-step-field",
        '{"steps":[{"group":"a","job":"one","desc":"d","description":"long","cmd":"true",'
        '"manifest":{"lane":"portable","category":"applications"},'
        '"result_manifests":[{"lane":"portable","category":"applications",'
        '"test":"applications/date","mode":"verify","backend":"ptrace"},'
        '{"kind":"structured-test-results","schema":2,'
        '"path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"a.one"}],'
        '"integration_test_binaries":["unit_alpha"],'
        '"deps":[],"env":{"K":"V"},"networkonly":false,"engine_only":false,"timeout":5,'
        '"cpu_timeout":3,"cmdtype":"generic-with-flag","jobs_flag":"-j","jobs_env":"J","explains":[],'
        '"fail_fast_family":"fam",'
        '"hint":{"resources":{},"est_duration_s":1.0,"classification":"light"}}]}',
    ),
    (
        # A cap of exactly 0 is documented as "blocked on purpose", NOT as a load error.
        "cap-of-zero-is-a-deliberate-block",
        '{"resource_caps":{"browser":0},"steps":[{"group":"a","job":"one","cmd":"true",'
        '"hint":{"resources":{"browser":1}}}]}',
    ),
    (
        # An intentionally-skipped step never launches, so its dormant demand cannot starve.
        "skipped-step-dormant-demand",
        '{"resource_caps":{"browser":1},"steps":[{"group":"a","job":"one","cmd":"true",'
        '"skip_reason":"empty-manifest-bucket","hint":{"resources":{"browser":9}}}]}',
    ),
    (
        # A diamond is not a cycle. The three-colour walk grey-lists a node while it is on the
        # path; a shared ancestor reached twice by different routes must not read as one.
        "diamond-is-not-a-cycle",
        '{"steps":[{"group":"g","job":"root","cmd":"true"},'
        '{"group":"g","job":"left","cmd":"true","deps":["g.root"]},'
        '{"group":"g","job":"right","cmd":"true","deps":["g.root"]},'
        '{"group":"g","job":"join","cmd":"true","deps":["g.left","g.right"]}]}',
    ),
)

#: Inspection subcommands. Validating in ALL of them is the point: a graph check that needs a run
#: is not a check, it is a build.
INSPECTION_SUBCOMMANDS: tuple[str, ...] = ("list", "plan", "ascii", "dot", "json", "yaml")


def compare_loader_graph_validation(py: list[str], rs: list[str], rep: Report) -> None:
    """Both builds must refuse the same broken graphs, with the same bytes, from every entry.

    THE CHEAP CHECK IS THE POINT. A `list` that prints a cyclic graph and exits 0 gives a caller
    no way to validate a DAG short of running it, and running it was, for a cycle, a crash. So the
    refusal is asserted on `run` AND on every inspection subcommand, with the same exit code.
    """
    with tempfile.TemporaryDirectory() as tmp:
        for label, doc, phrase in LOADER_REFUSALS:
            path = os.path.join(tmp, f"{label}.json")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(doc)
            for sub in ("run", *INSPECTION_SUBCOMMANDS):
                args = (sub, "--dag", path)
                po = run(py, args)
                ro = run(rs, args)
                case = f"loader-refusal:{label}:{sub}"
                if po.returncode != ro.returncode:
                    rep.bad(case, f"exit py={po.returncode} rs={ro.returncode}")
                elif po.returncode != 2:
                    rep.bad(
                        case,
                        f"a broken graph must be REFUSED at load (exit 2), not accepted or "
                        f"crashed; both builds exited {po.returncode}: {po.stderr!r}",
                    )
                elif phrase not in po.stderr or phrase not in ro.stderr:
                    rep.bad(
                        case,
                        f"the refusal must say {phrase!r}; py={po.stderr!r} rs={ro.stderr!r}",
                    )
                elif po.stderr != ro.stderr:
                    rep.bad(case, f"stderr py={po.stderr!r} rs={ro.stderr!r}")
                else:
                    rep.ok(case)

        for label, doc in LOADER_ACCEPTANCES:
            path = os.path.join(tmp, f"ok-{label}.json")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(doc)
            args = ("list", "--dag", path)
            po = run(py, args)
            ro = run(rs, args)
            case = f"loader-acceptance:{label}"
            if po.returncode != ro.returncode:
                rep.bad(case, f"exit py={po.returncode} rs={ro.returncode}")
            elif po.returncode != 0:
                rep.bad(
                    case,
                    f"this graph must still LOAD; both builds exited {po.returncode}: "
                    f"{po.stderr!r}",
                )
            elif po.stdout != ro.stdout:
                rep.bad(case, f"stdout py={po.stdout!r} rs={ro.stdout!r}")
            else:
                rep.ok(case)


def compare_uncontained_cpu_budget_notice(py: list[str], rs: list[str], rep: Report) -> None:
    """Both builds must describe the same best-effort uncontained CPU-time fallback.

    The capability manifest remains ``uncontained.cpu_timeout=false`` because procfs
    process-group accounting is only a lower bound, not a cgroup-equivalent guarantee. The
    scheduler nevertheless attempts that weaker guard. This check pins the operator-facing
    disclosure, its Python/Rust parity, and its once-per-run cardinality.
    """
    with tempfile.TemporaryDirectory() as tmp:
        live = os.path.join(tmp, "live.json")
        with open(live, "w", encoding="utf-8") as fh:
            fh.write(
                '{"steps": [{"group": "g", "job": "a", '
                '"cmd": "true", "cpu_timeout": 7, "timeout": 30}]}'
            )
        # This contract is deliberately about the uncontained scheduler.  An outer validation
        # graph lends this harness a real delegated cgroup, so --allow-cgroup-failure would quite
        # correctly stay boxed there.  The explicit opt-out selects the same route both at top
        # level and below that parent-owned containment boundary.
        po = run(py, ("run", "--dag", live, "--unsafe-no-cgroups"))
        ro = run(rs, ("run", "--dag", live, "--unsafe-no-cgroups"))
        label = "uncontained-cpu-budget-notice"
        phrase = "best-effort procfs process-group CPU floor"
        py_lines = [ln for ln in po.stderr.splitlines() if phrase in ln]
        rs_lines = [ln for ln in ro.stderr.splitlines() if phrase in ln]
        if not py_lines or not rs_lines:
            rep.bad(
                label,
                "an UNCONTAINED run must name the weaker CPU-time fallback; "
                f"py={py_lines!r} rs={rs_lines!r}",
            )
        elif py_lines != rs_lines:
            rep.bad(label, f"notice differs: py={py_lines!r} rs={rs_lines!r}")
        elif "largest 7s" not in py_lines[0]:
            rep.bad(label, f"the notice must quote the budget policed by the fallback: {py_lines[0]!r}")
        else:
            rep.ok(label)

        # Once per RUN. A multi-step graph is the case that separates a run-level notice from a
        # per-step one; a per-tick regression would show up as a much larger count still.
        many = os.path.join(tmp, "many.json")
        with open(many, "w", encoding="utf-8") as fh:
            fh.write(
                '{"steps": ['
                '{"group": "g", "job": "a", "cmd": "true", '
                '"cpu_timeout": 7, "timeout": 30}, '
                '{"group": "g", "job": "b", "cmd": "true", '
                '"cpu_timeout": 5, "timeout": 30}, '
                '{"group": "g", "job": "c", "cmd": "true", '
                '"cpu_timeout": 3, "timeout": 30}]}'
            )
        po = run(py, ("run", "--dag", many, "--unsafe-no-cgroups"))
        ro = run(rs, ("run", "--dag", many, "--unsafe-no-cgroups"))
        label = "uncontained-cpu-budget-notice:once-per-run"
        py_count = sum(1 for ln in po.stderr.splitlines() if phrase in ln)
        rs_count = sum(1 for ln in ro.stderr.splitlines() if phrase in ln)
        if (py_count, rs_count) != (1, 1):
            rep.bad(
                label,
                "the notice belongs once per RUN, not once per step or once per tick; "
                f"py={py_count} rs={rs_count}",
            )
        else:
            rep.ok(label)


#: How long the planted escapee lives. It MUST exceed `run`'s own expiry, or the case cannot
#: distinguish a build that finishes from one that blocks: a self-limiting escapee releases the
#: pipes on its own, the blocked build returns just before the harness gives up, and the two builds
#: agree on an exit code for the wrong reason. Measured against a build with the defect still in
#: place: at a 90s lifetime it returned at 90s and the check PASSED; the case only fails when the
#: survivor outlives the harness. The fixture kills the pid it recorded once the run is over, so a
#: long lifetime does not mean a long-lived leak.
_ESCAPEE_LIFETIME_S = 300


def compare_run_timeout(py: list[str], rs: list[str], rep: Report) -> None:
    """Both builds must bound the WHOLE run identically, and refuse a mis-ordered budget alike.

    THE BOUND THAT MATTERS IS THE ONE THAT STILL REPORTS. Per-step budgets cannot bound a run:
    any number of individually-legal steps can sum past any ceiling, and the only thing that
    stopped such a run was an external job kill, which discards the evidence it was supposed to
    explain. So the outer budget is checked in the scheduler's own loop, and this asserts both
    engines do it: the run must finish EARLY (well before the DAG's natural length), must not
    report success, and must not hit the harness timeout.

    The second half is the ordering contract made executable. A step allowed to run as long as
    the whole run can only ever be killed by the outer bound, which attributes the overrun to the
    run instead of to the node — so both builds must REFUSE to start rather than run with a
    budget whose breach cannot be attributed.
    """
    with tempfile.TemporaryDirectory() as tmp:
        # Natural length ~12s (three 4s steps, -s1); budget 6s; every step's own wall budget 5s,
        # strictly under the run budget, so the ordering is legal.
        bounded = os.path.join(tmp, "bounded.json")
        with open(bounded, "w", encoding="utf-8") as fh:
            fh.write(
                json.dumps(
                    {
                        "steps": [
                            {
                                "group": "a",
                                "job": name,
                                "cmd": "sleep 4",
                                "timeout": 5,
                                "cpu_timeout": 600,
                            }
                            for name in ("one", "two", "three")
                        ]
                    }
                )
            )
        # Same DAG with a step allowed to outlive the run: a mis-ordered budget.
        misordered = os.path.join(tmp, "misordered.json")
        with open(misordered, "w", encoding="utf-8") as fh:
            fh.write(
                json.dumps(
                    {
                        "steps": [
                            {
                                "group": "a",
                                "job": "wide",
                                "cmd": "sleep 1",
                                "timeout": 600,
                                "cpu_timeout": 600,
                            }
                        ]
                    }
                )
            )

        def _run(cmd: list[str], dag: str) -> Outcome:
            return run(
                cmd,
                (
                    "run", "--dag", dag, "-q", "-s", "1", "-j", "1", NOPROF, NOFB,
                    UNBOXED, "--run-timeout", "6",
                ),
            )

        po, ro = _run(py, bounded), _run(rs, bounded)
        label = "run-timeout"
        blocked = [n for n, o in (("py", po), ("rs", ro)) if o.returncode == 124]
        if blocked:
            rep.bad(
                label,
                f"{'/'.join(blocked)} never returned under an outer run budget "
                f"(exit py={po.returncode} rs={ro.returncode}); a bound that cannot report is "
                "the external job kill it exists to replace",
            )
        elif po.returncode != ro.returncode:
            rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}")
        elif po.returncode == 0:
            rep.bad(label, "a run cut short by its outer budget must not report success")
        else:
            rep.ok(label)

        pm, rm = _run(py, misordered), _run(rs, misordered)
        label = "run-timeout-misordered-refusal"
        if pm.returncode != rm.returncode:
            rep.bad(label, f"exit py={pm.returncode} rs={rm.returncode}")
        elif pm.returncode == 0:
            rep.bad(
                label,
                "a step allowed to run as long as the whole run must be REFUSED, not accepted: "
                "the outer bound would fire first and the overrun could not be attributed",
            )
        elif not all("REFUSING" in o.stdout + o.stderr for o in (pm, rm)):
            rep.bad(
                label,
                "both builds must SAY they refused and name the offending steps; "
                f"py={pm.stderr[-200:]!r} rs={rm.stderr[-200:]!r}",
            )
        else:
            rep.ok(label)


def compare_escapee_teardown(py: list[str], rs: list[str], rep: Report) -> None:
    """Both builds must FINISH and REAP a step whose child escapes its process group.

    THE CASE THIS COVERS, AND WHY ITS ABSENCE MATTERED. Every other `run` comparison here uses a
    step that dies when its process group is signalled, so all of them exercise the easy teardown
    path. A step can instead leave a `setsid` child behind: the child changes session and process
    group, the intermediate parent exits, and the survivor keeps the step's stdout/stderr write ends
    open. A build that waits unconditionally for those pipes to reach EOF never returns -- it does
    not fail, it hangs, and a run that hangs writes no results at all. Two builds can therefore
    disagree completely on this path while every other check agrees, which is exactly what happened:
    the suite reported full agreement across every fixture while the two runners behaved differently
    on the one behaviour that decides whether the system can report on its own failures.

    WHAT IS ASSERTED. Both builds must return the SAME exit status, neither may hit the harness
    timeout, AND the exact recorded escapee pid must be gone before fixture cleanup runs. The last
    assertion is load-bearing: merely closing the runner's pipe and returning would still pass an
    exit-code comparison while leaking work onto the host.

    The escapee bounds itself with a short sleep and is also killed by recorded pid afterwards, so
    the fixture cannot leak a process onto a shared machine even if a build leaves it running.
    """
    with tempfile.TemporaryDirectory() as tmp:
        # ONE PID FILE PER BUILD. Both runs plant an escapee, so a single shared file records only
        # the second one and the first is leaked onto the machine on every invocation. Found by
        # counting survivors after a full suite run, not by reading the code.
        def _case(tag: str, cmd: list[str]) -> tuple[str, Outcome]:
            pid_file = os.path.join(tmp, f"escapee-{tag}.pid")
            script = os.path.join(tmp, f"escapee-{tag}.sh")
            with open(script, "w", encoding="utf-8") as fh:
                fh.write(
                    "#!/usr/bin/env bash\n"
                    # setsid leaves the step's session AND process group, so a process-group kill
                    # cannot reach it; it inherits the pipes, so the readers never see EOF.
                    f"setsid bash -c 'echo $$ > {pid_file}; exec sleep {_ESCAPEE_LIFETIME_S}' &\n"
                    f"sleep {_ESCAPEE_LIFETIME_S}\n"
                )
            os.chmod(script, 0o755)
            dag_path = os.path.join(tmp, f"dag-{tag}.json")
            with open(dag_path, "w", encoding="utf-8") as fh:
                fh.write(
                    json.dumps(
                        {
                            "steps": [
                                {
                                    "group": "escapee",
                                    "job": "setsid_child",
                                    "cmd": f"bash {script}",
                                    "timeout": 5,
                                    "cpu_timeout": 600,
                                }
                            ]
                        }
                    )
                )
            return pid_file, run(
                cmd,
                (
                    "run", "--dag", dag_path, "-q", "-s", "1", "-j", "1",
                    NOPROF, NOFB, UNBOXED,
                ),
            )

        pid_files: list[str] = []
        survivors: dict[str, bool | None] = {}
        try:
            pf, po = _case("py", py)
            pid_files.append(pf)
            survivors["py"] = _recorded_pid_alive(pf)
            _kill_recorded_pid(pf)
            pf, ro = _case("rs", rs)
            pid_files.append(pf)
            survivors["rs"] = _recorded_pid_alive(pf)
        finally:
            for pf in pid_files:
                _kill_recorded_pid(pf)

        label = "escapee-teardown"
        blocked = [n for n, o in (("py", po), ("rs", ro)) if o.returncode == 124]
        if blocked:
            rep.bad(
                label,
                f"{'/'.join(blocked)} never returned after a step left a setsid child behind "
                f"(exit py={po.returncode} rs={ro.returncode}); a run that cannot finish cannot "
                "report what went wrong",
            )
        elif po.returncode != ro.returncode:
            rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}")
        elif po.returncode == 0:
            rep.bad(label, "a step killed at its wall budget must not report success")
        elif any(state is None for state in survivors.values()):
            rep.bad(label, f"fixture did not record both escapee pids: {survivors}")
        elif any(state for state in survivors.values()):
            leaked = "/".join(name for name, alive in survivors.items() if alive)
            rep.bad(label, f"{leaked} returned while its recorded setsid escapee was still alive")
        else:
            rep.ok(label)


def compare_term_attribution(py: list[str], rs: list[str], rep: Report) -> None:
    """A timed-out step gets a bounded SIGTERM opportunity to identify its in-flight work.

    This cross-check intentionally asserts the observable marker and bounded command completion,
    not an absolute sub-four-second CLI duration. Overall latency also includes repeated ``/proc``
    ownership sweeps and host scheduling, so that wall-clock heuristic falsely accused correct
    teardown on loaded builders. Paired unit tests pin the underlying early-exit rule directly:
    an unreaped zombie is not a live process-group member and therefore cannot consume the grace.
    """
    with tempfile.TemporaryDirectory() as tmp:
        dag_path = os.path.join(tmp, "dag.json")
        marker = "TERM_ATTRIBUTION_MARKER"
        with open(dag_path, "w", encoding="utf-8") as fh:
            fh.write(
                json.dumps(
                    {
                        "steps": [
                            {
                                "group": "timeout",
                                "job": "reports_term",
                                "cmd": (
                                    f"trap 'printf {marker}\\n >&2; exit 0' TERM; "
                                    "while :; do sleep 1; done"
                                ),
                                "timeout": 2,
                                "cpu_timeout": 600,
                            }
                        ]
                    }
                )
            )
        args = (
            "run",
            "--dag",
            dag_path,
            "-q",
            "-s",
            "1",
            "-j",
            "1",
            NOPROF,
            NOFB,
            "--unsafe-no-cgroups",
        )
        po, ro = run(py, args), run(rs, args)
        combined = {"py": po.stdout + po.stderr, "rs": ro.stdout + ro.stderr}
        label = "term-attribution"
        blocked = [name for name, out in (("py", po), ("rs", ro)) if out.returncode == 124]
        missing = [name for name, output in combined.items() if marker not in output]
        if blocked:
            rep.bad(label, f"{'/'.join(blocked)} did not return after its wall timeout")
        elif po.returncode != ro.returncode:
            rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}")
        elif po.returncode == 0:
            rep.bad(label, "a step that exceeded its wall budget reported success")
        elif missing:
            rep.bad(label, f"{'/'.join(missing)} suppressed the timed-out step's SIGTERM marker")
        else:
            rep.ok(label)


def _journal_step_end_ok(row: Mapping[str, object]) -> bool:
    """Read a journal verdict without accepting a truthy string as success."""
    value = row.get("ok")
    if type(value) is not bool:
        raise ValueError("journal step_end ok must be a boolean")
    return value


def compare_test_attribution_evidence(py: list[str], rs: list[str], rep: Report) -> None:
    """Explicit evidence and test-level culprit attribution are paired public behavior."""
    with tempfile.TemporaryDirectory() as tmp:
        dag_path = os.path.join(tmp, "attribution.json")
        Path(dag_path).write_text(
            json.dumps(
                {
                    "steps": [
                        {
                            "group": "tests",
                            "job": "suite",
                            "cmd": (
                                "printf '##TEST-START suite::alpha\\n'; "
                                "printf '##TEST-END suite::alpha ok\\n'; "
                                "printf '##TEST-START suite::gamma_the_hang\\n'; "
                                "trap 'exit 0' TERM; while :; do sleep 1; done"
                            ),
                            "timeout": 2,
                            "cpu_timeout": 600,
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )
        dirs = {name: os.path.join(tmp, name) for name in ("py", "rs")}
        args = (
            "run",
            "--dag",
            dag_path,
            "-q",
            "-s",
            "1",
            "-j",
            "1",
            NOPROF,
            NOFB,
            "--unsafe-no-cgroups",
        )
        outcomes = {
            "py": run(py, args, {"DAGRUN_LOG_DIR": dirs["py"]}),
            "rs": run(rs, args, {"DAGRUN_LOG_DIR": dirs["rs"]}),
        }
        phrase = "culprit test suite::gamma_the_hang"
        if any(out.returncode == 0 or phrase not in out.stdout + out.stderr for out in outcomes.values()):
            rep.bad("attribution:culprit", f"py={outcomes['py']}\nrs={outcomes['rs']}")
        else:
            rep.ok("attribution:culprit")

        def normalized(
            directory: str,
        ) -> list[tuple[str, str, str, str, str, str, bool | None]]:
            rows = [
                json.loads(line)
                for line in Path(directory, "journal.jsonl").read_text(encoding="utf-8").splitlines()
            ]
            return [
                (
                    str(row.get("event", "")),
                    str(row.get("step", "")),
                    str(row.get("test", "")),
                    str(row.get("verdict", "")),
                    str(row.get("tests_started", "")),
                    str(row.get("tests_completed", "")),
                    _journal_step_end_ok(row) if row.get("event") == "step_end" else None,
                )
                for row in rows
                if row.get("event") in {"test_start", "test_end", "step_end"}
            ]

        try:
            try:
                _journal_step_end_ok({"event": "step_end", "ok": "false"})
            except ValueError:
                pass
            else:
                raise ValueError("journal reader accepted a string step_end verdict")
            py_rows, rs_rows = normalized(dirs["py"]), normalized(dirs["rs"])
            logs = {
                name: Path(directory, "tests.suite.log").read_text(encoding="utf-8")
                for name, directory in dirs.items()
            }
            private = all(
                stat.S_IMODE(Path(directory, filename).stat().st_mode) == 0o600
                for directory in dirs.values()
                for filename in ("journal.jsonl", "tests.suite.log")
            )
        except (OSError, ValueError, json.JSONDecodeError) as exc:
            rep.bad("attribution:evidence", str(exc))
        else:
            if py_rows != rs_rows:
                rep.bad("attribution:evidence", f"journal py={py_rows!r} rs={rs_rows!r}")
            elif not private or any("suite::gamma_the_hang" not in log for log in logs.values()):
                rep.bad("attribution:evidence", f"private={private} logs={logs!r}")
            else:
                rep.ok("attribution:evidence")


def compare_step_log_ceiling(py: list[str], rs: list[str], rep: Report) -> None:
    """The per-step durable-log ceiling truncates at the SAME byte in both engines.

    Byte-for-byte, deliberately. Both the cut position and the truncation marker are public
    evidence-file content, and the marker is a long literal duplicated in two languages: without
    a paired check, one engine can be edited and the drift is invisible until someone diffs two
    incidents' logs. Compared here where a divergence is a failing check, not a footnote.
    """
    with tempfile.TemporaryDirectory() as tmp:
        dag_path = os.path.join(tmp, "log-ceiling.json")
        Path(dag_path).write_text(
            json.dumps(
                {
                    "steps": [
                        {
                            "group": "flood",
                            "job": "stdout",
                            # Far past the 100-byte ceiling set below, and emitted in several
                            # writes so the cut lands mid-stream rather than on a chunk boundary.
                            "cmd": "for i in $(seq 1 40); do printf 'ABCDEFGHIJ'; done",
                            "timeout": 30,
                            "cpu_timeout": 600,
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )
        dirs = {name: os.path.join(tmp, name) for name in ("py", "rs")}
        args = (
            "run", "--dag", dag_path, "-q", "-s", "1", "-j", "1",
            NOPROF, NOFB, "--unsafe-no-cgroups",
        )
        outs = {
            name: run(
                cmd,
                args,
                {
                    "DAGRUN_LOG_DIR": dirs[name],
                    "DAGRUN_LOG_MAX_BYTES": "100",
                },
            )
            for name, cmd in (("py", py), ("rs", rs))
        }
        label = "attribution:log-ceiling"
        if outs["py"].returncode != outs["rs"].returncode:
            rep.bad(label, f"exit py={outs['py'].returncode} rs={outs['rs'].returncode}")
            return
        try:
            logs = {
                name: Path(directory, "flood.stdout.log").read_bytes()
                for name, directory in dirs.items()
            }
            journals = {
                name: Path(directory, "journal.jsonl").read_text(encoding="utf-8")
                for name, directory in dirs.items()
            }
        except OSError as exc:
            rep.bad(label, str(exc))
            return
        # Drop `ts`: it is a wall-clock reading, so it legitimately differs between two runs.
        truncations = {
            name: [
                {k: v for k, v in row.items() if k != "ts"}
                for row in (json.loads(line) for line in text.splitlines())
                if row.get("event") == "step_log_truncated"
            ]
            for name, text in journals.items()
        }
        if logs["py"] != logs["rs"]:
            rep.bad(label, f"capped log py={logs['py']!r} rs={logs['rs']!r}")
        elif logs["py"][:100] != b"ABCDEFGHIJ" * 10 or len(logs["py"]) <= 100:
            rep.bad(label, f"ceiling did not bite: {logs['py']!r}")
        elif truncations["py"] != truncations["rs"]:
            rep.bad(label, f"journal py={truncations['py']!r} rs={truncations['rs']!r}")
        elif len(truncations["py"]) != 1:
            rep.bad(label, f"expected exactly one truncation record; got {truncations['py']!r}")
        else:
            rep.ok(label)


def compare_capture_ceiling(py: list[str], rs: list[str], rep: Report) -> None:
    """The IN-MEMORY capture ceiling drops the same bytes and says so identically in both engines.

    This is the check the archived work could not have: the property is a MEMORY property, so
    nothing about a normal run reveals it. Its one observable consequence is the failure dump --
    which tail survived, and the notice that says how much did not -- so that is what is compared,
    byte for byte, on a step whose output is far past the ceiling.

    Without this pairing, bounding one engine's capture and not the other's creates a divergence
    that no existing check can see: both runs still pass, both still print a dump, and only the
    engine that OOMs under a real runaway tells you which one was fixed.

    THE STEP FLOODS BOTH PIPES, and that is not incidental. A stdout-only fixture cannot see the
    divergence that a per-stream ring produces -- one ring per pipe holds TWICE the ceiling and
    announces the drop once per pipe, each notice counting only its own half -- so it would have
    passed while the two engines disagreed about both numbers in the notice and about how much of
    the step the runner was holding. The step writes its whole stdout burst, pauses so the order
    is settled rather than raced, then its whole stderr burst: the surviving tail is then pure
    stderr in both engines, so "the earlier stdout is gone" is a stable assertion about a single
    step-wide ring rather than a statement about thread scheduling.
    """
    with tempfile.TemporaryDirectory() as tmp:
        dag_path = os.path.join(tmp, "capture-ceiling.json")
        Path(dag_path).write_text(
            json.dumps(
                {
                    "steps": [
                        {
                            "group": "flood",
                            "job": "both",
                            # Far past the 300-byte ceiling set below, emitted in many writes so
                            # the cut lands mid-stream, and FAILING so the dump is printed.
                            "cmd": (
                                'for i in $(seq 1 200); do echo "line$i"; done; sleep 0.5; '
                                'for i in $(seq 1 200); do echo "oops$i" >&2; done; exit 3'
                            ),
                            "timeout": 30,
                            "cpu_timeout": 600,
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )
        args = (
            "run", "--dag", dag_path, "-s", "1", "-j", "1",
            NOPROF, NOFB, "--unsafe-no-cgroups",
        )
        outs = {
            name: run(
                cmd,
                args,
                {
                    "DAGRUN_NO_STEP_LOGS": "1",
                    "DAGRUN_CAPTURE_MAX_BYTES": "300",
                },
            )
            for name, cmd in (("py", py), ("rs", rs))
        }
        label = "attribution:capture-ceiling"
        if outs["py"].returncode != outs["rs"].returncode:
            rep.bad(label, f"exit py={outs['py'].returncode} rs={outs['rs'].returncode}")
            return

        def detail(text: str) -> list[str]:
            lines = text.splitlines()
            try:
                start = next(
                    i for i, line in enumerate(lines) if line.endswith("----- detail -----")
                )
                end = next(
                    i for i, line in enumerate(lines) if line.endswith("----- end detail -----")
                )
            except StopIteration:
                return []
            return lines[start + 1 : end]

        dumps = {name: detail(out.stdout) for name, out in outs.items()}
        # 1492 bytes of "lineN" plus 1492 of "oopsN": the ceiling is over the STEP, so the notice
        # counts both pipes. A per-stream ring says 1492, twice.
        notice = (
            "[dagrun] EARLIER OUTPUT DROPPED: this step produced 2984 bytes but "
            "only the last 300 were kept in memory (raise or lift the ceiling with "
            "DAGRUN_CAPTURE_MAX_BYTES; 0 = unlimited). The durable per-step log is "
            "unaffected and still has the rest."
        )
        notices = {
            name: [line for line in dump if "EARLIER OUTPUT DROPPED" in line]
            for name, dump in dumps.items()
        }
        if dumps["py"] != dumps["rs"]:
            rep.bad(label, f"detail py={dumps['py']!r} rs={dumps['rs']!r}")
        elif not dumps["py"]:
            rep.bad(label, "neither engine printed a failure detail block")
        elif len(notices["py"]) != 1 or len(notices["rs"]) != 1:
            # ONE ceiling, ONE notice: a per-pipe notice reports neither what the step produced
            # nor what survived, and holds twice the bytes the operator asked for.
            rep.bad(label, f"expected exactly one dropped-notice: {notices!r}")
        elif dumps["py"][0] != f"[flood.both] {notice}":
            rep.bad(label, f"notice line was {dumps['py'][0]!r}")
        elif "[flood.both] oops200" not in dumps["py"]:
            rep.bad(label, "the TAIL of the output was not retained")
        elif any(line.startswith("[flood.both] line") for line in dumps["py"]):
            # The stdout burst is 1492 bytes ending 1492 bytes before the end, so a step-wide
            # 300-byte ring cannot have kept ANY of it. A ring per pipe keeps 300 bytes of it.
            rep.bad(label, "the ceiling did not bite: stdout survived a stderr-filled tail")
        else:
            rep.ok(label)


def compare_box_subcommand(py: list[str], rs: list[str], rep: Report) -> None:
    """`box` boxes ONE command identically in both engines, argv quoting included.

    A new user-visible subcommand in one engine and not the other is exactly the parity gap this
    repository's `make both` / `make cross` contract exists to prevent, and it is the stated
    reason the archived Rust-only implementation was never landed. The flag inventory is covered
    by `compare_cli_schema`; what is compared here is BEHAVIOUR: the tag the step is given, the
    label derivation, and -- the part most likely to diverge silently -- that argv is shell-quoted
    element by element rather than joined, so an argument containing a space, a quote, a `;` or a
    `$(...)` survives as ONE argument instead of becoming shell syntax.
    """
    label = "box:subcommand"
    # Deliberately hostile argv: a space, an embedded single quote, a shell metacharacter, and a
    # command substitution. Correct quoting echoes them back verbatim.
    hostile = ["a b", "it's", "semi;colon", "$(echo pwned)"]
    # `--mem 512M` IS PART OF THE FIXTURE, not decoration. It is the value #82, the user guide and
    # both `--help` texts all show, and it reaches the run as `--max-mem`, which is checked
    # against the graph's MODELED footprint. Both engines floored that model at 8 GiB and so
    # REFUSED (exit 2) every `--mem` below the default the flag exists to lower. Boxing one
    # command with a small memory ceiling is the whole feature; a fixture that omits the ceiling
    # cannot see it broken, and cannot see it broken in one engine only.
    outs = {
        name: run(
            cmd,
            (
                "box",
                "--allow-cgroup-failure",
                "--label",
                "probe",
                "--mem",
                "512M",
                "--timeout",
                "60",
                "--cores",
                "1",
                "--",
                "printf",
                "[%s]",
                *hostile,
            ),
            {"DAGRUN_PROFILE_DIR": os.path.join(tempfile.gettempdir(), "cross-box")},
            inherit_runner_authority=True,
        )
        for name, cmd in (("py", py), ("rs", rs))
    }
    if outs["py"].returncode != outs["rs"].returncode:
        rep.bad(label, f"exit py={outs['py'].returncode} rs={outs['rs'].returncode}")
        return
    if outs["py"].returncode != 0:
        rep.bad(label, f"box exited {outs['py'].returncode}\n{outs['py'].stdout}")
        return
    expected = "".join(f"[{part}]" for part in hostile)
    for name, out in outs.items():
        if f"[box.probe] \u2713 PASS" not in out.stdout:
            rep.bad(label, f"{name}: no PASS line for box.probe\n{out.stdout}")
            return
        if expected not in out.stdout:
            rep.bad(label, f"{name}: argv was not passed through verbatim: {out.stdout!r}")
            return

    # An empty command is a usage error in both, and says so rather than boxing nothing.
    empty = {name: run(cmd, ("box",)) for name, cmd in (("py", py), ("rs", rs))}
    if empty["py"].returncode != 2 or empty["rs"].returncode != 2:
        rep.bad(
            label,
            f"empty box exit py={empty['py'].returncode} rs={empty['rs'].returncode}",
        )
    elif not all("no command given" in out.stderr for out in empty.values()):
        rep.bad(label, f"empty box stderr py={empty['py'].stderr!r} rs={empty['rs'].stderr!r}")
    else:
        rep.ok(label)


def compare_memory_admission(py: list[str], rs: list[str], rep: Report) -> None:
    """Both engines give the SAME admission verdict, in the same words, with the same exit code.

    Admission is cross-process shared state: the two editions read and write ONE ledger on the
    host, so they have to agree about its schema, about the arithmetic, and about what each
    verdict is called. A Python-only implementation would not merely be a missing feature, it
    would be a runner that ignores reservations the other engine is honouring -- which is worse
    than no admission at all, because the ledger would then be actively misleading.

    The host limits are pinned through the operator overrides so the comparison is about the
    DECISION and not about whatever this machine happens to have free.
    """
    label = "admission:verdicts"
    with tempfile.TemporaryDirectory() as tmp:
        dag_path = os.path.join(tmp, "admission.json")
        Path(dag_path).write_text(
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
        gib = 1024**3
        base_no_admission = (
            "run", "--dag", dag_path, "-s", "1", "-j", "1",
            NOPROF, NOFB, "--unsafe-no-cgroups",
        )
        base = (*base_no_admission, "--admission")

        def env(name: str, budget: int, headroom: int) -> dict[str, str]:
            return {
                "DAGRUN_MEM_LEDGER": os.path.join(tmp, f"{name}.json"),
                "DAGRUN_ADMISSION_BUDGET_BYTES": str(budget),
                "DAGRUN_ADMISSION_HEADROOM_BYTES": str(headroom),
            }

        # 1. REFUSED: the request alone is bigger than the whole-host budget.
        refused = {
            name: run(cmd, (*base, "--max-mem", "64G"), env("refuse", 16 * gib, 64 * gib))
            for name, cmd in (("py", py), ("rs", rs))
        }
        # 2. QUEUED on host memory a non-runner tenant is holding.
        queued = {
            name: run(cmd, (*base, "--max-mem", "32G"), env("queue", 64 * gib, 1024))
            for name, cmd in (("py", py), ("rs", rs))
        }
        # 3. GRANTED, and the run then proceeds to completion.
        granted = {
            name: run(cmd, (*base, "--max-mem", "16G"), env("grant", 64 * gib, 64 * gib))
            for name, cmd in (("py", py), ("rs", rs))
        }
        # 4. Admission without a number to reserve is a usage error, not a guess.
        no_number = {
            name: run(cmd, base, env("nonum", 64 * gib, 64 * gib))
            for name, cmd in (("py", py), ("rs", rs))
        }
        # 5. ALREADY ADMITTED, PROVEN BY THE LEDGER. Cgroup boxing re-execs the runner into a
        # systemd scope with `execvp`, which keeps the pid AND the /proc start time, so the
        # pre-exec reservation is still this run's own and both engines must SKIP admission: an
        # engine that asks again counts one run twice, and on a tight budget queues the run behind
        # itself with nothing that can ever release it. The seed below writes exactly that record
        # -- this pid, this start time -- and then `exec`s the engine, which keeps both.
        own_ledger = os.path.join(tmp, "own-record.json")
        seed = os.path.join(tmp, "seed_own_record.py")
        Path(seed).write_text(
            "import json, sys, time\n"
            "pid = int(sys.argv[1])\n"
            "stat = open(f'/proc/{pid}/stat', encoding='utf-8').read()\n"
            "starttime = int(stat[stat.rfind(')') + 2:].split()[19])\n"
            "json.dump({'admissions': [{'pid': pid, 'starttime': starttime,\n"
            "    'bytes': 16 * 1024**3, 'tag': 'run', 'ts': time.time()}]},\n"
            "    open(sys.argv[2], 'w', encoding='utf-8'))\n",
            encoding="utf-8",
        )
        # `$$` is the shell's own pid, and `exec` hands that very pid to the engine -- the same
        # identity preservation the boxing re-exec relies on.
        seeded_shell = (
            f'{shlex.quote(sys.executable)} {shlex.quote(seed)} "$$" '
            f'{shlex.quote(own_ledger)} && exec "$@"'
        )
        own_record = {
            name: run(
                ["sh", "-c", seeded_shell, "sh", *cmd],
                (*base, "--max-mem", "16G"),
                {
                    "DAGRUN_MEM_LEDGER": own_ledger,
                    "DAGRUN_ADMISSION_BUDGET_BYTES": str(gib),
                    "DAGRUN_ADMISSION_HEADROOM_BYTES": str(64 * gib),
                    "DAGRUN_IN_SCOPE": "1",
                },
            )
            for name, cmd in (("py", py), ("rs", rs))
        }
        # 5b. AN INHERITED SENTINEL IS NOT AN ADMISSION. `systemd-run --setenv=DAGRUN_IN_SCOPE=1`
        # sets the variable for the WHOLE scope and every step inherits it, so a runner invoked as
        # a step of a boxed run reads the same "1" while holding nothing. An engine that skips on
        # the flag alone reserves nothing, prints no verdict and exits 0 -- here, for a 16 GiB
        # request against a 1 GiB budget on an EMPTY ledger. Both engines must refuse it instead.
        inherited = {
            name: run(
                cmd,
                (*base, "--max-mem", "16G"),
                {**env("inherited", gib, 64 * gib), "DAGRUN_IN_SCOPE": "1"},
            )
            for name, cmd in (("py", py), ("rs", rs))
        }
        # 5c. THE DEFAULT BUDGET, MEASURED RATHER THAN OVERRIDDEN. Every other leg pins both host
        # limits through the operator overrides, which makes the two engines agree about the
        # DECISION while saying nothing about the fraction of MemTotal each of them would claim on
        # a real host. A request larger than any machine forces both to print that number.
        default_budget = {
            name: run(
                cmd,
                (*base, "--max-mem", "8000000G"),
                {"DAGRUN_MEM_LEDGER": os.path.join(tmp, "default.json")},
            )
            for name, cmd in (("py", py), ("rs", rs))
        }
        # 5d. A WAIT_S NEITHER ENGINE CAN HONOUR IS A USAGE ERROR IN BOTH, with the same ceiling
        # in the message. 1e19 used to be accepted by both validators and then ABORT the Rust
        # engine (exit 101, overflow when adding a duration to an instant) while Python ran on.
        # `-1` pins the other half: argparse eats a negative number as the flag's VALUE, so an
        # engine that left the token unconsumed would diagnose the same typo differently.
        too_long = {
            name: run(
                cmd,
                (*base_no_admission, "--max-mem", "16G", "--admission=1e19"),
                env("long", 64 * gib, 64 * gib),
            )
            for name, cmd in (("py", py), ("rs", rs))
        }
        negative = {
            name: run(cmd, (*base, "-1", "--max-mem", "16G"), env("neg", 64 * gib, 64 * gib))
            for name, cmd in (("py", py), ("rs", rs))
        }
        # A day is the ceiling, and it is INCLUSIVE -- a granted run does not wait at all.
        at_ceiling = {
            name: run(cmd, (*base, "86400", "--max-mem", "16G"), env("ceil", 64 * gib, 64 * gib))
            for name, cmd in (("py", py), ("rs", rs))
        }
        # 6. A ledger neither engine can read stops the run instead of waving it through: the
        # ledger IS the shared state, and guessing past it silently restores the contention.
        corrupt_path = os.path.join(tmp, "corrupt.json")
        Path(corrupt_path).write_text("{not json at all", encoding="utf-8")
        corrupt = {
            name: run(
                cmd,
                (*base, "--max-mem", "16G"),
                {
                    "DAGRUN_MEM_LEDGER": corrupt_path,
                    "DAGRUN_ADMISSION_BUDGET_BYTES": str(64 * gib),
                    "DAGRUN_ADMISSION_HEADROOM_BYTES": str(64 * gib),
                },
            )
            for name, cmd in (("py", py), ("rs", rs))
        }

        # (name, outcomes, expected exit, phrase both engines must print, phrase is on stderr)
        checks: list[tuple[str, dict[str, Outcome], int, str, bool]] = [
            ("refuse", refused, 4, "REFUSED: 64.0 GiB exceeds the whole-host budget", True),
            ("queue", queued, 4, "QUEUED on HOST MEMORY held outside this tool", True),
            ("grant", granted, 0, "GRANTED 16.0 GiB", False),
            ("no-max-mem", no_number, 2, "--admission requires --max-mem", True),
            ("in-scope-own-record", own_record, 0, "DELIBERATELY UNBOXED", True),
            (
                "in-scope-inherited-sentinel",
                inherited,
                4,
                "REFUSED: 16.0 GiB exceeds the whole-host budget of 1.0 GiB",
                True,
            ),
            (
                "default-budget",
                default_budget,
                4,
                "exceeds the whole-host budget",
                True,
            ),
            (
                "wait-above-ceiling",
                too_long,
                2,
                "--admission WAIT_S must be a finite number of seconds in [0, 86400] (got '1e19')",
                True,
            ),
            (
                "wait-negative",
                negative,
                2,
                "--admission WAIT_S must be a finite number of seconds in [0, 86400] (got '-1')",
                True,
            ),
            ("wait-at-ceiling", at_ceiling, 0, "GRANTED 16.0 GiB", False),
            ("corrupt-ledger", corrupt, 4, "admission ledger unusable", True),
        ]
        for name, outs, expected_code, phrase, on_stderr in checks:
            if outs["py"].returncode != outs["rs"].returncode:
                rep.bad(
                    label,
                    f"{name}: exit py={outs['py'].returncode} rs={outs['rs'].returncode}",
                )
                return
            if outs["py"].returncode != expected_code:
                rep.bad(
                    label,
                    f"{name}: expected exit {expected_code}, got {outs['py'].returncode}\n"
                    f"{outs['py'].stdout}{outs['py'].stderr}",
                )
                return
            for engine, out in outs.items():
                text = out.stderr if on_stderr else out.stdout
                if phrase not in text:
                    rep.bad(label, f"{name}: {engine} did not say {phrase!r}\n{text}")
                    return
        # The own-record leg is defined by what it must NOT do, so say that outright rather than
        # leaning on exit 0: a re-admitting engine would print its verdict on the way past, and
        # would leave a SECOND record in the ledger for one run.
        for engine, out in own_record.items():
            noise = out.stdout + out.stderr
            if "[admission]" in noise or "REFUSED" in noise:
                rep.bad(
                    label,
                    f"in-scope-own-record: {engine} re-admitted after the boxing re-exec\n{noise}",
                )
                return
        held = json.loads(Path(own_ledger).read_text(encoding="utf-8"))["admissions"]
        if len(held) != 1:
            rep.bad(label, f"in-scope-own-record: one run left {len(held)} records: {held}")
            return
        # The measured default budget is the same number in both engines, to the byte. This is
        # what an unpinned budget FRACTION would break: the decision legs above would stay green
        # while the two editions disagreed about how much of a real host they may hold.
        if default_budget["py"].stderr != default_budget["rs"].stderr:
            rep.bad(
                label,
                "default-budget: the two engines measured different host budgets\n"
                f"--- py ---\n{default_budget['py'].stderr}"
                f"--- rs ---\n{default_budget['rs'].stderr}",
            )
            return
        rep.ok(label)


def compare_batch_teardown_grace(py: list[str], rs: list[str], rep: Report) -> None:
    """Eager cancellation grants one diagnostic window, not one window per sibling.

    Use an outer run timeout as a behavioral tripwire instead of treating shared-host wall time as
    a performance SLO. Eight serial five-second TERM graces cannot finish before that tripwire,
    while one shared grace has ample room even under scheduler and `/proc`-scan contention.
    """
    with tempfile.TemporaryDirectory() as tmp:
        dag_path = os.path.join(tmp, "batch-grace.json")
        steps: list[dict[str, object]] = [
            {
                "group": "batch",
                "job": "fails",
                "cmd": "sleep 0.5; false",
                "timeout": 20,
                "cpu_timeout": 600,
            }
        ]
        steps.extend(
            {
                "group": "batch",
                "job": f"resists_{index}",
                "cmd": "trap '' TERM; sleep 30",
                "timeout": 20,
                "cpu_timeout": 600,
            }
            for index in range(8)
        )
        Path(dag_path).write_text(json.dumps({"steps": steps}), encoding="utf-8")
        args = (
            "run",
            "--dag",
            dag_path,
            "-q",
            "-s",
            "9",
            "-j",
            "9",
            "--run-timeout",
            "25",
            NOPROF,
            NOFB,
            "--unsafe-no-cgroups",
        )
        po, ro = run(py, args), run(rs, args)
        evidence = (po.stdout + po.stderr, ro.stdout + ro.stderr)
        if (
            po.returncode == ro.returncode != 0
            and all("[scheduler] RUN TIMEOUT" not in text for text in evidence)
            and max(po.elapsed_s, ro.elapsed_s) < 32.0
        ):
            rep.ok("teardown:shared-batch-grace")
        else:
            rep.bad(
                "teardown:shared-batch-grace",
                f"exit py={po.returncode} rs={ro.returncode}; "
                f"elapsed py={po.elapsed_s:.3f}s rs={ro.elapsed_s:.3f}s",
            )


def _recorded_pid_alive(pid_file: str) -> bool | None:
    """Whether the exact fixture pid still exists as a non-zombie; None means no pid was recorded."""
    try:
        with open(pid_file, encoding="utf-8") as fh:
            pid = int(fh.read().strip())
    except (OSError, ValueError):
        return None
    try:
        stat = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
    except OSError:
        return False
    close = stat.rfind(")")
    return close >= 0 and stat[close + 2 :].split()[0] != "Z"


def _kill_recorded_pid(pid_file: str) -> None:
    """SIGKILL the pid this fixture recorded, if it is still alive.

    Kills one EXPLICIT pid that the fixture itself spawned -- never a name or command-line pattern,
    which on a shared machine would reach other tenants' work.
    """
    try:
        with open(pid_file, encoding="utf-8") as fh:
            pid = int(fh.read().strip())
    except (OSError, ValueError):
        return  # never started, or already cleaned up
    try:
        os.kill(pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        pass  # already gone, which is the expected outcome once a build reaps correctly


def compare_fixture(py: list[str], rs: list[str], fx: Fixture, rep: Report) -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dag_path = os.path.join(tmp, "dag.json")
        with open(dag_path, "w", encoding="utf-8") as fh:
            fh.write(json.dumps(fx.dag))

        # 1-2) static commands: list/ascii/dot byte-identical, json parsed-equal.
        _static_parity(py, rs, dag_path, fx.name, rep)

        # 3) run: default concurrent exit-code parity. The default eager-exit path is
        # deterministic in its EXIT CODE (1 iff any reachable step fails) but NOT in the
        # passed/aborted split, which depends on which in-flight step is cancelled first, so
        # only the exit code is compared here.
        #
        # All generic `run` comparisons explicitly select --unsafe-no-cgroups: cgroup-v2 boxing is
        # environment-dependent and cannot be asserted byte-identically here (see cross/README.md).
        # This makes both builds run the SAME observable UNBOXED scheduling core even when the
        # differential itself was given a delegated cgroup. Dedicated cases below exercise boxing.
        concurrent_args = (
            "run", "--dag", dag_path, "-q", "-s", "4", "-j", "64", NOPROF, NOFB, UNBOXED,
        )
        po = run(py, concurrent_args)
        ro = run(rs, concurrent_args)
        label = f"{fx.name}/run(default-exit)"
        if po.returncode != ro.returncode:
            rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}")
        else:
            rep.ok(label)

        # 4) run serial (-s1): deterministic counts + exit code. Keep an ample -j64 aggregate CPU
        # budget so serial scheduling does not rewrite an authored inner width or its jobs_flag.
        # With one step at a time the
        # ready-set loop dispatches in a single deterministic LPT sequence, so the
        # passed/failed/aborted/intentionally-skipped/dependency-skipped/not-launched counts are
        # fully reproducible between the two builds.
        # (Run under --keep-going: with it, a failure no longer stops the launch of independent
        # work, so both builds account for EVERY node rather than leaving a race-dependent tail in
        # the not-launched bucket.)
        serial_args = (
            "run", "--dag", dag_path, "-q", "-s", "1", "-j", "64", "-k", NOPROF, NOFB,
            UNBOXED,
        )
        po = run(py, serial_args)
        ro = run(rs, serial_args)
        label = f"{fx.name}/run(serial-counts)"
        pc, rc = _counts(po.stderr), _counts(ro.stderr)
        if po.returncode != ro.returncode:
            rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}")
        elif pc is None or rc is None:
            rep.bad(label, f"missing summary counts py={po.stderr!r} rs={ro.stderr!r}")
        elif pc != rc:
            rep.bad(label, f"counts py={pc} rs={rc}")
        else:
            rep.ok(label)

        # 5) --max-mem sizing decision, when the fixture supplies a budget.
        if fx.max_mem is not None:
            po = run(
                py,
                (
                    "run", "--dag", dag_path, "-q", "-k", "--max-mem", fx.max_mem,
                    NOPROF, NOFB, UNBOXED,
                ),
            )
            ro = run(
                rs,
                (
                    "run", "--dag", dag_path, "-q", "-k", "--max-mem", fx.max_mem,
                    NOPROF, NOFB, UNBOXED,
                ),
            )
            label = f"{fx.name}/sizing"
            ps, rss = _sizing(po.stderr), _sizing(ro.stderr)
            prefusal, rrefusal = _sizing_refusal(po.stderr), _sizing_refusal(ro.stderr)
            if (
                po.returncode == ro.returncode == 2
                and prefusal is not None
                and prefusal == rrefusal
            ):
                rep.ok(label)
            elif ps is None or rss is None:
                rep.bad(label, f"missing sizing line py={po.stderr!r} rs={ro.stderr!r}")
            elif ps != rss:
                rep.bad(label, f"(--max-steps, footprint, budget) py={ps} rs={rss}")
            else:
                rep.ok(label)

        # 6) --selected parity: the named step and its dependency ancestry must agree on exit code
        # and the passed/failed/aborted/intentionally-skipped/dependency-skipped/not-launched
        # counts across both builds. Serial execution makes the counts reproducible even though
        # full-DAG timing is not.
        selection = _first_selection(fx)
        if selection is not None:
            tag, expected_nodes = selection
            selected_args = (
                "run", "--dag", dag_path, "-q", "-s", "1", "-j", "64", "--selected", tag,
                NOPROF, NOFB, UNBOXED,
            )
            po = run(py, selected_args)
            ro = run(rs, selected_args)
            label = f"{fx.name}/selected({tag})"
            pc, rc = _counts(po.stderr), _counts(ro.stderr)
            if po.returncode != ro.returncode:
                rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}")
            elif pc is None or rc is None:
                rep.bad(label, f"missing summary counts py={po.stderr!r} rs={ro.stderr!r}")
            elif pc != rc:
                rep.bad(label, f"counts py={pc} rs={rc}")
            elif sum(pc) != expected_nodes:
                rep.bad(label, f"expected {expected_nodes} accounted nodes; got counts {pc}")
            else:
                rep.ok(label)


def _store_csv_names(directory: str) -> list[str]:
    """Sorted basenames of the ``*.csv`` files in a profile-store directory (``[]`` if absent)."""
    if not os.path.isdir(directory):
        return []
    return sorted(name for name in os.listdir(directory) if name.endswith(".csv"))


def _header_and_eol(path: str) -> tuple[str, str]:
    """The first line (header, with any trailing ``\\r`` stripped) and the line-ending STYLE
    (``"CRLF"`` / ``"LF"`` / ``"none"``) of a CSV file, read as raw bytes so the terminator is
    observed exactly (never newline-translated)."""
    with open(path, "rb") as fh:
        data = fh.read()
    eol = "CRLF" if b"\r\n" in data else ("LF" if b"\n" in data else "none")
    header = data.split(b"\n", 1)[0].rstrip(b"\r").decode("utf-8")
    return header, eol


def _profile_width_records(directory: str) -> list[tuple[str, str]]:
    """Read ``(inner_jobs, container_class)`` from the one per-step profile CSV."""

    names = [name for name in _store_csv_names(directory) if name.startswith("step_profiles_")]
    if len(names) != 1:
        return []
    with open(os.path.join(directory, names[0]), newline="", encoding="utf-8") as handle:
        return [
            (row.get("inner_jobs", ""), row.get("container_class", ""))
            for row in csv.DictReader(handle)
        ]


def _ambient_width_from_container_class(container_class: str) -> int | None:
    """Mirror the conservative whole-core budget encoded in a profile identity."""

    matched = re.fullmatch(
        r"affinity([0-9]+)_cpu-max-(max|unknown|([0-9]+)_([0-9]+))",
        container_class,
    )
    if matched is None:
        return None
    affinity = int(matched.group(1))
    quota_text = matched.group(2)
    if quota_text in {"max", "unknown"}:
        return max(1, affinity)
    quota = int(matched.group(3))
    period = int(matched.group(4))
    if period <= 0:
        return None
    return max(1, min(affinity, quota // period))


def compare_profile_store(py: list[str], rs: list[str], rep: Report) -> None:
    """Assert the auto-logging profile STORE (Feature D) has an identical on-disk schema in both
    builds. Runs the SAME tiny DAG under each build with ``--perf-dir`` into a fresh temp dir
    (explicitly unboxed, so it is environment-independent), then asserts the two
    stores agree on: (a) the SET of CSV filenames (proving ``machine_id`` + ``container_class``, and
    hence ``nproc``, agree), (b) each file's HEADER row byte-for-byte, and (c) the line-ending
    style. It also proves that explicitly unboxed undeclared steps retain the same positive ambient
    width in both engines instead of claiming a cgroup default that was not enforced; boxed
    default-width behavior is pinned by each engine's scheduler tests. Other data cells are not
    compared because timestamps/elapsed/git-SHA differ."""
    dag = (
        '{"steps": [{"group": "g", "job": "a", "cmd": "true"}, '
        '{"group": "g", "job": "b", "cmd": "true", "deps": ["g.a"]}]}'
    )
    with tempfile.TemporaryDirectory() as tmp:
        dag_path = os.path.join(tmp, "dag.json")
        with open(dag_path, "w", encoding="utf-8") as fh:
            fh.write(dag)
        py_dir = os.path.join(tmp, "py_store")
        rs_dir = os.path.join(tmp, "rs_store")
        po = run(
            py,
            ("run", "--dag", dag_path, "-q", "-s", "1", "-j", "4", "--perf-dir", py_dir,
             NOFB, "--unsafe-no-cgroups"),
        )
        ro = run(
            rs,
            ("run", "--dag", dag_path, "-q", "-s", "1", "-j", "4", "--perf-dir", rs_dir,
             NOFB, "--unsafe-no-cgroups"),
        )
        label = "profile-store"
        if po.returncode != ro.returncode:
            rep.bad(label, f"run exit py={po.returncode} rs={ro.returncode}")
            return
        py_names = _store_csv_names(py_dir)
        rs_names = _store_csv_names(rs_dir)
        if py_names != rs_names:
            rep.bad(
                label,
                f"profile-store filename sets differ (machine_id/container_class/nproc mismatch)\n"
                f"--- py ---\n{py_names}\n--- rs ---\n{rs_names}",
            )
            return
        if not py_names:
            rep.bad(label, "no profile CSVs were written by either build")
            return
        py_records = _profile_width_records(py_dir)
        rs_records = _profile_width_records(rs_dir)
        expected = (
            _ambient_width_from_container_class(py_records[0][1])
            if py_records
            else None
        )
        valid_ambient = (
            len(py_records) == 2
            and expected is not None
            and all(width == str(expected) for width, _container in py_records)
        )
        if py_records != rs_records or not valid_ambient:
            rep.bad(
                f"{label}:unboxed-ambient-inner-jobs",
                f"unboxed undeclared steps must record their identity-derived ambient width "
                f"{expected!r}; py={py_records!r} rs={rs_records!r}",
            )
        else:
            rep.ok(f"{label}:unboxed-ambient-inner-jobs")
        for name in py_names:
            py_hdr, py_eol = _header_and_eol(os.path.join(py_dir, name))
            rs_hdr, rs_eol = _header_and_eol(os.path.join(rs_dir, name))
            sub = f"{label}:{name}"
            if py_eol != rs_eol:
                rep.bad(sub, f"line endings differ py={py_eol} rs={rs_eol}")
            elif py_hdr != rs_hdr:
                rep.bad(sub, f"header differs\n--- py ---\n{py_hdr}\n--- rs ---\n{rs_hdr}")
            else:
                rep.ok(sub)

        # The compatibility promise is stronger than matching headers: either reader must be able
        # to ingest a store emitted by either writer and reduce it to the same typed summary. This
        # protects historical data when deployments switch implementations.
        commands = {"py": py, "rs": rs}
        for producer, store in (("py", py_dir), ("rs", rs_dir)):
            summaries = {
                reader: run(command, ("summary", "build", "--perf-dir", store))
                for reader, command in commands.items()
            }
            label = f"profile-store:{producer}-writer-cross-read"
            if any(outcome.returncode != 0 for outcome in summaries.values()):
                rep.bad(label, f"py={summaries['py']}\nrs={summaries['rs']}")
            elif summaries["py"].stdout != summaries["rs"].stdout:
                rep.bad(
                    label,
                    "readers derived different summaries from the same emitted store\n"
                    f"--- py ---\n{summaries['py'].stdout}\n"
                    f"--- rs ---\n{summaries['rs'].stdout}",
                )
            else:
                try:
                    document = json.loads(summaries["py"].stdout)
                    buckets = document.get("buckets", [])
                    tags = {
                        bucket.get("step")
                        for bucket in buckets
                        if isinstance(bucket, dict)
                    }
                    samples = sum(
                        len(bucket.get("samples", []))
                        for bucket in buckets
                        if isinstance(bucket, dict)
                        and isinstance(bucket.get("samples"), list)
                    )
                except (json.JSONDecodeError, AttributeError, TypeError):
                    tags, samples = set(), 0
                if tags != {"g.a", "g.b"} or samples != 2:
                    rep.bad(
                        label,
                        f"cross-read summary was vacuous or incomplete: tags={tags!r}, "
                        f"samples={samples}",
                    )
                else:
                    rep.ok(label)


def compare_mixed_profile_writers(py: list[str], rs: list[str], rep: Report) -> None:
    """Exercise simultaneous Python/Rust writers against one shared local profile store."""

    dag = (
        '{"steps":['
        '{"group":"g","job":"a","cmd":"true"},'
        '{"group":"g","job":"b","cmd":"true"},'
        '{"group":"g","job":"c","cmd":"true"}'
        "]}"
    )
    with tempfile.TemporaryDirectory(prefix="dagrun-mixed-profile-") as tmp:
        dag_path = os.path.join(tmp, "dag.json")
        Path(dag_path).write_text(dag, encoding="utf-8")
        store = os.path.join(tmp, "store")
        args = (
            "run",
            "--dag",
            dag_path,
            "--perf-dir",
            store,
            NOFB,
            "--unsafe-no-cgroups",
            "--max-steps",
            "1",
            "-q",
        )
        commands = (py, rs, py, rs)
        with ThreadPoolExecutor(max_workers=len(commands)) as executor:
            outcomes = list(executor.map(lambda command: run(command, args), commands))
        if any(outcome.returncode != 0 for outcome in outcomes):
            rep.bad(
                "profile-store:mixed-concurrent-writers",
                "\n".join(f"run {index}: {outcome}" for index, outcome in enumerate(outcomes)),
            )
            return

        step_paths = sorted(Path(store).glob("step_profiles_*.csv"))
        whole_paths = sorted(
            path
            for path in Path(store).glob("*.csv")
            if not path.name.startswith("step_profiles_")
        )
        try:
            if len(step_paths) != 1 or len(whole_paths) != 1:
                raise ValueError(
                    f"expected one step and one whole-run CSV, got {step_paths!r}, {whole_paths!r}"
                )
            with step_paths[0].open(newline="", encoding="utf-8") as handle:
                step_rows = list(csv.DictReader(handle))
            with whole_paths[0].open(newline="", encoding="utf-8") as handle:
                whole_rows = list(csv.DictReader(handle))
        except (OSError, ValueError, csv.Error) as error:
            rep.bad("profile-store:mixed-concurrent-writers", str(error))
            return
        if len(step_rows) != 12 or len(whole_rows) != 4:
            rep.bad(
                "profile-store:mixed-concurrent-writers",
                f"lost rows: step={len(step_rows)}/12 whole={len(whole_rows)}/4",
            )
            return

        summaries = {
            "py": run(py, ("summary", "build", "--perf-dir", store)),
            "rs": run(rs, ("summary", "build", "--perf-dir", store)),
        }
        if (
            any(outcome.returncode != 0 for outcome in summaries.values())
            or summaries["py"].stdout != summaries["rs"].stdout
        ):
            rep.bad(
                "profile-store:mixed-concurrent-writers",
                f"mixed store did not cross-read identically\npy={summaries['py']}\n"
                f"rs={summaries['rs']}",
            )
        else:
            rep.ok("profile-store:mixed-concurrent-writers")


# --------------------------------------------------------------------- profile run identity


def _run_id_shape(directory: str) -> list[int] | None:
    """The GROUPING the ``run_id`` column induces over a store's rows, as run indices.

    A ``run_id`` is opaque (nanoseconds + PID) so its literal value cannot be compared across two
    builds, and comparing headers — all :func:`compare_profile_store` does — cannot see a column's
    values at all. What a reader actually depends on IS comparable: which rows the column says
    belong to the same DAG execution. Two rows in one group are asserted to have run at the same
    time (their offsets share an origin), so the grouping is the whole semantic content.

    Returns run indices in first-appearance order (``[0, 0]`` = one run of two steps, ``[0, 1, 2]``
    = three separate runs), or ``None`` when the store is unreadable or any ``run_id`` is blank.
    """
    names = [name for name in _store_csv_names(directory) if name.startswith("step_profiles_")]
    if len(names) != 1:
        return None
    with open(os.path.join(directory, names[0]), newline="", encoding="utf-8") as handle:
        ids = [row.get("run_id", "") for row in csv.DictReader(handle)]
    if not ids or any(not value for value in ids):
        return None
    seen: dict[str, int] = {}
    return [seen.setdefault(value, len(seen)) for value in ids]


def compare_profile_run_identity(py: list[str], rs: list[str], rep: Report) -> None:
    """Assert both builds agree on WHICH ROWS the ``run_id`` column calls one DAG execution.

    Header parity cannot see this: the header comes from the column-list constant, so an edition
    could mint one id per run, per batch or per process and still pass. The two shapes below are
    the ones the documented reconstruction rests on:

    * ``run``: every step of one execution shares one id (``[0, 0]``), even though the rows are
      flushed by the same sink, so an id minted per BATCH would split a single run into groups
      that never overlap by construction.
    * ``sweep``: every iteration is its OWN execution with its own Runner and its own monotonic
      origin, so it must get its own id (``[0, 1, 2]``). One shared id across a strictly
      sequential sweep makes the documented rule — same ``run_id`` rows overlap iff their
      ``[started, finished]`` intervals do — declare all three concurrent.
    """
    with tempfile.TemporaryDirectory(prefix="run-identity-cross-") as tmp:
        run_dag = os.path.join(tmp, "run.json")
        with open(run_dag, "w", encoding="utf-8") as handle:
            handle.write(
                '{"steps": [{"group": "g", "job": "a", "cmd": "true"}, '
                '{"group": "g", "job": "b", "cmd": "true", "deps": ["g.a"]}]}'
            )
        for label, args_for, expected in (
            (
                "profile-run-identity:one-run-one-id",
                lambda store: (
                    "run", "--dag", run_dag, "-q", "-s", "1", "-j", "2",
                    "--perf-dir", store, NOFB, "--unsafe-no-cgroups",
                ),
                [0, 0],
            ),
            (
                "profile-run-identity:sweep-per-execution",
                lambda store: (
                    "sweep", "--dag", run_dag, "--step", "g.a", "--jobs", "1..3",
                    "--perf-dir", store, "--unsafe-no-cgroups",
                ),
                [0, 1, 2],
            ),
        ):
            py_dir = os.path.join(tmp, f"py-{label.rsplit(':', 1)[1]}")
            rs_dir = os.path.join(tmp, f"rs-{label.rsplit(':', 1)[1]}")
            po = run(py, args_for(py_dir))
            ro = run(rs, args_for(rs_dir))
            if po.returncode != 0 or ro.returncode != 0:
                rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}\n{po}\n{ro}")
                continue
            py_shape = _run_id_shape(py_dir)
            rs_shape = _run_id_shape(rs_dir)
            if py_shape != expected or rs_shape != expected:
                rep.bad(
                    label,
                    f"run_id grouping must be {expected}; py={py_shape} rs={rs_shape}",
                )
            else:
                rep.ok(label)


# --------------------------------------------------------------------- profile time-series trace


_TRACE_COLUMNS = (
    "timestamp",
    "machine_id",
    "container_class",
    "git_sha",
    "outer_jobs",
    "profile_base_sha",
    "enforcement_kind",
    "runner_name",
    "run_id",
    "step",
    "inner_jobs",
    "sample_index",
    "sample_kind",
    "elapsed_s",
    "interval_s",
    "cpu_usage_s",
    "user_s",
    "sys_s",
    "effective_cores",
    "user_cores",
    "system_cores",
    "throttled_s",
    "interval_throttled_s",
    "thread_count",
)

_TRACE_SWEEP_COLUMNS = (
    "sweep_id",
    "sweep_logical_cpus",
    "sweep_mode",
    "sweep_pass",
    "sweep_physical_cores",
    "sweep_repeat",
    "sweep_sample",
    "sweep_target_s",
    "sweep_width_source",
    "workload_digest",
)

_TRACE_SECONDS6 = frozenset(
    {
        "elapsed_s",
        "interval_s",
        "cpu_usage_s",
        "user_s",
        "sys_s",
        "throttled_s",
        "interval_throttled_s",
    }
)
_TRACE_CORES4 = frozenset({"effective_cores", "user_cores", "system_cores"})
_TRACE_FIXED_DECIMAL_RE = {
    4: re.compile(r"^[0-9]+\.[0-9]{4}$"),
    6: re.compile(r"^[0-9]+\.[0-9]{6}$"),
}
_TRACE_TIMESTAMP_RE = re.compile(
    r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$"
)


def _only_trace_path(directory: str) -> Path:
    traces = Path(directory) / "traces"
    paths = sorted(path for path in traces.glob("*.csv") if path.is_file())
    if len(paths) != 1:
        raise ValueError(f"expected one trace CSV under {traces}, found {paths}")
    return paths[0]


def _trace_cell(row: Mapping[str, str | None], column: str, row_number: int) -> str:
    value = row.get(column)
    if value is None:
        raise ValueError(f"trace row {row_number} has no {column!r} cell")
    return value


def _normalized_trace_csv(path: Path) -> tuple[str, tuple[str, ...], tuple[tuple[str, ...], ...]]:
    """Validate one sweep trace and erase only values that are inherently run-specific.

    The returned rows retain stable provenance and blank-vs-present structure. Decimal values are
    replaced by precision tokens only after their exact fixed-width encoding has been checked, so
    two executions need not consume identical CPU time to prove that their CSV contracts match.
    """

    raw = path.read_bytes()
    eol = "CRLF" if b"\r\n" in raw else ("LF" if b"\n" in raw else "none")
    with path.open(newline="", encoding="utf-8") as handle:
        reader = csv.DictReader(handle)
        header = tuple(reader.fieldnames or ())
        rows = list(reader)
    expected_header = (*_TRACE_COLUMNS, *_TRACE_SWEEP_COLUMNS)
    if header != expected_header:
        raise ValueError(f"trace header differs: expected {expected_header!r}, got {header!r}")
    if eol != "LF":
        raise ValueError(f"trace must use LF line endings, got {eol}")
    if len(rows) < 3:
        raise ValueError(f"trace needs start, periodic and final rows, got {len(rows)}")

    kinds = [_trace_cell(row, "sample_kind", number) for number, row in enumerate(rows, start=2)]
    if kinds[0] != "start" or kinds[-1] != "final" or any(
        kind != "periodic" for kind in kinds[1:-1]
    ):
        raise ValueError(f"trace sample order is not start/periodic*/final: {kinds!r}")
    indices = [
        int(_trace_cell(row, "sample_index", number))
        for number, row in enumerate(rows, start=2)
    ]
    if indices != list(range(len(rows))):
        raise ValueError(f"trace sample indices are not contiguous from zero: {indices!r}")
    for row_number, row in enumerate(rows, start=2):
        for column in _TRACE_SECONDS6:
            value = _trace_cell(row, column, row_number)
            if value and _TRACE_FIXED_DECIMAL_RE[6].fullmatch(value) is None:
                raise ValueError(
                    f"trace row {row_number} {column} must be blank or fixed to 6 decimals, "
                    f"got {value!r}"
                )
        for column in _TRACE_CORES4:
            value = _trace_cell(row, column, row_number)
            if value and _TRACE_FIXED_DECIMAL_RE[4].fullmatch(value) is None:
                raise ValueError(
                    f"trace row {row_number} {column} must be blank or fixed to 4 decimals, "
                    f"got {value!r}"
                )
        thread_count = _trace_cell(row, "thread_count", row_number)
        if thread_count and (not thread_count.isascii() or not thread_count.isdigit()):
            raise ValueError(
                f"trace row {row_number} thread_count must be blank or an integer, "
                f"got {thread_count!r}"
            )
    elapsed = [
        float(_trace_cell(row, "elapsed_s", number))
        for number, row in enumerate(rows, start=2)
    ]
    if any(current <= previous for previous, current in zip(elapsed, elapsed[1:])):
        raise ValueError(f"trace elapsed times are not strictly increasing: {elapsed!r}")

    timestamps = {
        _trace_cell(row, "timestamp", number) for number, row in enumerate(rows, start=2)
    }
    if len(timestamps) != 1 or not _TRACE_TIMESTAMP_RE.fullmatch(next(iter(timestamps))):
        raise ValueError(f"trace batch timestamp is not one ISO second value: {timestamps!r}")
    run_ids = {_trace_cell(row, "run_id", number) for number, row in enumerate(rows, start=2)}
    if len(run_ids) != 1 or not next(iter(run_ids)):
        raise ValueError(f"trace rows do not share one non-empty run_id: {run_ids!r}")
    if path.stem != next(iter(run_ids)):
        raise ValueError(f"trace filename {path.stem!r} does not match row run_id {run_ids!r}")
    sweep_ids = {
        _trace_cell(row, "sweep_id", number) for number, row in enumerate(rows, start=2)
    }
    if len(sweep_ids) != 1 or not next(iter(sweep_ids)):
        raise ValueError(f"trace rows do not share one non-empty sweep_id: {sweep_ids!r}")

    representatives = (rows[0], rows[kinds.index("periodic")], rows[-1])
    normalized: list[tuple[str, ...]] = []
    for row_number, row in enumerate(representatives, start=1):
        cells: list[str] = []
        for column in header:
            value = _trace_cell(row, column, row_number)
            if column == "timestamp":
                cells.append("<timestamp>")
            elif column in {"run_id", "sweep_id"}:
                cells.append(f"<{column}>")
            elif column == "sample_index":
                cells.append("<sample_index>")
            elif column in _TRACE_SECONDS6:
                cells.append("<seconds6>" if value else "")
            elif column in _TRACE_CORES4:
                cells.append("<cores4>" if value else "")
            elif column == "thread_count":
                cells.append("<integer>" if value else "")
            else:
                cells.append(value)
        normalized.append(tuple(cells))
    return eol, header, tuple(normalized)


def compare_profile_timeseries_trace(py: list[str], rs: list[str], rep: Report) -> None:
    """Compare one real boxed sweep trace after normalizing only volatile measurements."""

    if _parent_offers_only_unboxed_delegation():
        print(
            "cross[dagrun]: SKIP time-series trace differential: "
            "the outer scheduler has no cgroup subtree to delegate"
        )
        rep.skip(
            "profile-timeseries",
            "boxing",
            "the outer scheduler has no cgroup subtree to delegate",
        )
        return

    with tempfile.TemporaryDirectory(prefix="dagrun-trace-cross-") as tmp:
        dag_path = os.path.join(tmp, "trace.json")
        Path(dag_path).write_text(
            json.dumps(
                {
                    "steps": [
                        {
                            "group": "g",
                            "job": "trace",
                            "cmd": "sleep 0.30; true $DAGRUN_EXTRA_ARGS",
                            "cmdtype": "generic-dash-j-command",
                            "timeout": 30,
                            "cpu_timeout": 30,
                            "hint": {"hard_mem_max_bytes": 268435456},
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )
        stores = {name: os.path.join(tmp, name) for name in ("py", "rs")}

        def args_for(store: str) -> tuple[str, ...]:
            return (
                "sweep",
                "--dag",
                dag_path,
                "--target-time",
                "0",
                "--jobs",
                "1",
                "--profile-timeseries",
                "50ms",
                "--perf-dir",
                store,
            )
        extra = {"DAGRUN_NO_STEP_LOGS": "1", "DAGRUN_FORCE_SCOPE_ATTEMPT": "1"}
        outcomes = {
            "py": run(
                py,
                args_for(stores["py"]),
                extra,
                inherit_runner_authority=True,
            ),
            "rs": run(
                rs,
                args_for(stores["rs"]),
                extra,
                inherit_runner_authority=True,
            ),
        }
        unavailable = {
            name: _boxing_capability_unavailable(outcome) for name, outcome in outcomes.items()
        }
        if all(unavailable.values()):
            print(
                "cross[dagrun]: SKIP time-series trace differential: "
                "cgroup-v2 + a working systemd --user scope are unavailable"
            )
            rep.skip(
                "profile-timeseries",
                "boxing",
                "cgroup-v2 + a working systemd --user scope are unavailable",
            )
            return
        if any(unavailable.values()) or any(outcome.returncode != 0 for outcome in outcomes.values()):
            rep.bad(
                "profile-timeseries",
                f"py={outcomes['py']}\nrs={outcomes['rs']}",
            )
            return
        try:
            normalized = {
                name: _normalized_trace_csv(_only_trace_path(store))
                for name, store in stores.items()
            }
        except (OSError, ValueError, csv.Error) as error:
            rep.bad("profile-timeseries", str(error))
            return
        if normalized["py"] != normalized["rs"]:
            rep.bad(
                "profile-timeseries",
                "normalized trace CSV differs\n"
                f"--- py ---\n{normalized['py']!r}\n--- rs ---\n{normalized['rs']!r}",
            )
        else:
            rep.ok("profile-timeseries:schema-and-encoding")


# --------------------------------------------------------------------------- plan feedback

#: A DAG whose steps carry only est_duration HINTS (no rss baselines) so the SYNTHETIC store below
#: is the sole source of learned durations + memory estimates. Its shape makes the two planners
#: disagree: g.prep is cheap on its own but heads a long chain (bottom-level 1+8), so critical-path
#: dispatches it first while greedy-lpt dispatches the individually-largest g.heavy/g.solo first.
_FEEDBACK_DAG = {
    "mem_cap_factor": 1.0,
    "mem_cap_floor_bytes": 0,
    "outer_mem_safety_factor": 1.0,
    "steps": [
        {"group": "g", "job": "prep", "desc": "prep", "cmd": "true",
         "hint": {"est_duration_s": 1.0}},
        {"group": "g", "job": "heavy", "desc": "heavy", "cmd": "true", "deps": ["g.prep"],
         "hint": {"est_duration_s": 10.0}},
        {"group": "g", "job": "solo", "desc": "solo", "cmd": "true",
         "hint": {"est_duration_s": 5.0}},
    ],
}

#: A fixed synthetic per-step profile store for :data:`_FEEDBACK_DAG`. g.heavy's 20s sample was
#: taken under 60% other-work contention, so the reader must DISCOUNT it back to ~8s (matching the
#: uncontended 8s sample) — proving contention-discounted median duration recovery. peak_bytes give
#: the memory model its rss estimates (6 GiB for heavy/solo => a tight --max-mem budget throttles to
#: --max-steps 1). Written with the pinned SYNTH identity so the file name matches what the reader
#: loads.
_FEEDBACK_STORE_CSV = (
    "timestamp,machine_id,container_class,git_sha,outer_jobs,profile_base_sha,enforcement_kind,"
    "runner_name,step,classification,inner_jobs,elapsed_s,returncode,ok,timed_out,oom_kills,"
    "peak_bytes,thread_peak,pct_other\n"
    f"2026-07-26T10:00:00,{SYNTH_MACHINE},{SYNTH_CONTAINER},abc,1,abc,unverified,local,"
    "g.prep,light,1,1.000,0,True,False,0,1073741824,,0.0\n"
    f"2026-07-26T10:00:01,{SYNTH_MACHINE},{SYNTH_CONTAINER},abc,1,abc,unverified,local,"
    "g.prep,light,1,1.100,0,True,False,0,1073741824,,0.0\n"
    f"2026-07-26T10:00:02,{SYNTH_MACHINE},{SYNTH_CONTAINER},abc,1,abc,unverified,local,"
    "g.heavy,cpu-bound,1,8.000,0,True,False,0,6442450944,,0.0\n"
    f"2026-07-26T10:00:03,{SYNTH_MACHINE},{SYNTH_CONTAINER},abc,1,abc,unverified,local,"
    "g.heavy,cpu-bound,1,20.000,0,True,False,0,6442450944,,60.0\n"
    f"2026-07-26T10:00:04,{SYNTH_MACHINE},{SYNTH_CONTAINER},abc,1,abc,unverified,local,"
    "g.solo,light,1,5.000,0,True,False,0,6442450944,,0.0\n"
    f"2026-07-26T10:00:05,{SYNTH_MACHINE},{SYNTH_CONTAINER},abc,1,abc,unverified,local,"
    "g.solo,light,1,5.200,0,True,False,0,6442450944,,0.0\n"
)


#: A DAG whose two steps carry only large est_duration HINTS, so the hostile store below (whose
#: numeric cells the reader must parse identically in both builds) is the sole source of the learned
#: durations/rss. If either build failed to parse a hostile cell the way the other does, the store
#: estimate — and thus the plan JSON — would diverge.
_HOSTILE_DAG = {
    "mem_cap_factor": 1.0,
    "mem_cap_floor_bytes": 0,
    "outer_mem_safety_factor": 1.0,
    "steps": [
        {"group": "h", "job": "ws", "desc": "ws", "cmd": "true",
         "hint": {"est_duration_s": 99.0}},
        {"group": "h", "job": "us", "desc": "us", "cmd": "true",
         "hint": {"est_duration_s": 99.0, "rss_baseline_bytes": 4242}},
    ],
}

#: A per-step profile store full of HOSTILE numeric cells that Python's ``float()``/``int()`` accept
#: but Rust's ``str::parse`` rejects (or vice versa) unless both builds normalize identically:
#: whitespace-padded ``elapsed_s`` and ``pct_other`` (must TRIM and then apply the discount in both),
#: PEP-515 underscore separators (must be REJECTED in both), and an out-of-i64 ``peak_bytes`` (must
#: be REJECTED in both — Python's arbitrary-precision int would otherwise keep it). This is the
#: adversarial data the earlier clean-only fixture could not exercise; it guards parse-helper
#: equivalence across the two builds.
#:
#: Expected (identical) derivation in BOTH builds:
#:   h.ws elapsed [' 8.0 '->8, 4.0, '10.0' under ' 50.0 '% -> 5] => robust median 5.000 (n=3); peaks
#:        [1000,2000,3000] => p90 3000.
#:   h.us elapsed ['1_0.0' rejected, 4.0, 6.0] => two surviving samples cannot MAD-reject an outlier,
#:        so the robust estimate is the MINIMUM => 4.000 (n<3); peaks ['1_000' rejected,
#:        9999999999999999999999 rejected, 5000] => p90 5000.
_HOSTILE_STORE_CSV = (
    "timestamp,machine_id,container_class,git_sha,outer_jobs,profile_base_sha,enforcement_kind,"
    "runner_name,step,classification,inner_jobs,elapsed_s,returncode,ok,timed_out,oom_kills,"
    "peak_bytes,thread_peak,pct_other\n"
    f"2026-07-26T10:00:00,{SYNTH_MACHINE},{SYNTH_CONTAINER},abc,1,abc,unverified,local,"
    "h.ws,light,1, 8.0 ,0,True,False,0,1000,,0.0\n"
    f"2026-07-26T10:00:01,{SYNTH_MACHINE},{SYNTH_CONTAINER},abc,1,abc,unverified,local,"
    "h.ws,light,1,4.0,0,True,False,0,2000,,0.0\n"
    f"2026-07-26T10:00:02,{SYNTH_MACHINE},{SYNTH_CONTAINER},abc,1,abc,unverified,local,"
    "h.ws,light,1,10.0,0,True,False,0,3000,, 50.0 \n"
    f"2026-07-26T10:00:03,{SYNTH_MACHINE},{SYNTH_CONTAINER},abc,1,abc,unverified,local,"
    "h.us,light,1,1_0.0,0,True,False,0,1_000,,0.0\n"
    f"2026-07-26T10:00:04,{SYNTH_MACHINE},{SYNTH_CONTAINER},abc,1,abc,unverified,local,"
    "h.us,light,1,4.0,0,True,False,0,9999999999999999999999,,0.0\n"
    f"2026-07-26T10:00:05,{SYNTH_MACHINE},{SYNTH_CONTAINER},abc,1,abc,unverified,local,"
    "h.us,light,1,6.0,0,True,False,0,5000,,0.0\n"
)


def compare_hostile_numeric_cells(py: list[str], rs: list[str], rep: Report) -> None:
    """Prove the profile-store numeric-cell PARSING is equivalent across builds on ADVERSARIAL data.

    Feeds a fixed store whose cells stress the Python-``float()``/``int()`` vs Rust-``str::parse``
    difference (leading/trailing whitespace, PEP-515 ``_`` separators, out-of-i64 magnitudes) and
    asserts ``plan --format json`` is BYTE-IDENTICAL across the two builds — plus that the derived
    values actually reflect the trim/reject rules (so the check proves the normalization happened,
    not merely that both builds fell back to the hint). This is the regression guard the earlier
    clean-only feedback fixture could not provide."""
    extra = {
        "DAGRUN_MACHINE_ID": SYNTH_MACHINE,
        "DAGRUN_CONTAINER_CLASS": SYNTH_CONTAINER,
    }
    with tempfile.TemporaryDirectory() as tmp:
        store = os.path.join(tmp, "store")
        os.makedirs(store, exist_ok=True)
        csv_name = f"step_profiles_{SYNTH_MACHINE}_{SYNTH_CONTAINER}.csv"
        with open(os.path.join(store, csv_name), "w", encoding="utf-8") as fh:
            fh.write(_HOSTILE_STORE_CSV)
        dag_path = os.path.join(tmp, "dag.json")
        with open(dag_path, "w", encoding="utf-8") as fh:
            fh.write(json.dumps(_HOSTILE_DAG))

        args = ("plan", "--dag", dag_path, "--perf-dir", store, "--format", "json")
        po = run(py, args, extra)
        ro = run(rs, args, extra)
        if po.returncode != 0 or ro.returncode != 0:
            rep.bad(
                "hostile-cells",
                f"exit py={po.returncode} rs={ro.returncode} "
                f"(py stderr={po.stderr!r}; rs stderr={ro.stderr!r})",
            )
            return
        if po.stdout != ro.stdout:
            rep.bad(
                "hostile-cells",
                f"plan output not byte-identical on hostile numeric cells\n--- py ---\n{po.stdout}\n"
                f"--- rs ---\n{ro.stdout}",
            )
            return
        rep.ok("hostile-cells")
        # Positive proof the whitespace-trim + reject rules actually fired (both agree, checked py):
        #   both hostile steps must derive their est_duration FROM THE STORE (not the 99.0 hint).
        #   h.ws has 3 surviving samples -> robust median 5.000; h.us has 2 -> the small-sample
        #   estimator returns the minimum 4.000 (see estimates._robust_median). Distinct values also
        #   prove BOTH steps were read (not one value coincidentally matched twice).
        if (
            '"est_source": "store"' in po.stdout
            and po.stdout.count('"est_duration_s": "5.000"') == 1
            and po.stdout.count('"est_duration_s": "4.000"') == 1
        ):
            rep.ok("hostile-cells:trim-applied")
        else:
            rep.bad(
                "hostile-cells:trim-applied",
                "expected h.ws to learn est_duration 5.000 and h.us 4.000 from the trimmed store "
                f"cells; got\n{po.stdout}",
            )


def _order_from_plan_json(stdout: str) -> list[str] | None:
    """Extract the scheduled ``order`` list from ``plan --format json`` output (``None`` if
    unparseable)."""
    try:
        obj = json.loads(stdout)
    except json.JSONDecodeError:
        return None
    if not isinstance(obj, dict):
        return None
    order = obj.get("order")
    if not isinstance(order, list) or not all(isinstance(x, str) for x in order):
        return None
    return [str(x) for x in order]


def compare_plan_feedback(py: list[str], rs: list[str], rep: Report) -> None:
    """Prove the profile-store FEEDBACK loop + the ``--planner`` choices agree across builds.

    Against a FIXED synthetic store (pinned to a host-independent machine/container identity):

    * ``plan`` output is BYTE-IDENTICAL across the two builds for BOTH planners and BOTH formats —
      so the derived estimates (contention-discounted median duration, high-percentile rss) and the
      dispatch order match exactly.
    * the ``critical-path`` scheduled order DIFFERS from ``greedy-lpt`` (the planner actually
      reorders), while both remain identical across builds.
    * ``plan`` with feedback OFF (``--no-profile-feedback``) is also byte-identical (hint-only path).
    * the memory-aware ``--max-mem`` sizing decision, now fed the store's rss estimates, matches
      across builds AND throttles below the CPU count (proving the store feeds the memory model).
    """
    extra = {
        "DAGRUN_MACHINE_ID": SYNTH_MACHINE,
        "DAGRUN_CONTAINER_CLASS": SYNTH_CONTAINER,
    }
    with tempfile.TemporaryDirectory() as tmp:
        store = os.path.join(tmp, "store")
        os.makedirs(store, exist_ok=True)
        csv_name = f"step_profiles_{SYNTH_MACHINE}_{SYNTH_CONTAINER}.csv"
        with open(os.path.join(store, csv_name), "w", encoding="utf-8") as fh:
            fh.write(_FEEDBACK_STORE_CSV)
        dag_path = os.path.join(tmp, "dag.json")
        with open(dag_path, "w", encoding="utf-8") as fh:
            fh.write(json.dumps(_FEEDBACK_DAG))

        orders: dict[str, list[str]] = {}
        for planner in ("greedy-lpt", "critical-path"):
            for fmt in ("text", "json"):
                po = run(
                    py,
                    ("plan", "--dag", dag_path, "--perf-dir", store, "--planner", planner,
                     "--format", fmt),
                    extra,
                )
                ro = run(
                    rs,
                    ("plan", "--dag", dag_path, "--perf-dir", store, "--planner", planner,
                     "--format", fmt),
                    extra,
                )
                label = f"plan-feedback:{planner}/{fmt}"
                if po.returncode != 0 or ro.returncode != 0:
                    rep.bad(label, f"exit py={po.returncode} rs={ro.returncode} "
                                   f"(py stderr={po.stderr!r}; rs stderr={ro.stderr!r})")
                    continue
                if po.stdout != ro.stdout:
                    rep.bad(
                        label,
                        f"plan output not byte-identical\n--- py ---\n{po.stdout}\n"
                        f"--- rs ---\n{ro.stdout}",
                    )
                    continue
                rep.ok(label)
                if fmt == "json":
                    order = _order_from_plan_json(po.stdout)
                    if order is None:
                        rep.bad(f"plan-feedback:{planner}/order", "unparseable order")
                    else:
                        orders[planner] = order

        # The two planners must actually produce a DIFFERENT scheduled order (else --planner is a
        # no-op on this fixture). Both builds already agree per-planner (asserted above).
        if "greedy-lpt" in orders and "critical-path" in orders:
            if orders["greedy-lpt"] == orders["critical-path"]:
                rep.bad(
                    "plan-feedback:planner-differs",
                    f"greedy-lpt and critical-path produced the SAME order {orders['greedy-lpt']}",
                )
            else:
                rep.ok("plan-feedback:planner-differs")

        # Feedback OFF: hint-only plan must still be byte-identical across builds.
        po = run(py, ("plan", "--dag", dag_path, "--no-profile-feedback", "--format", "json"), extra)
        ro = run(rs, ("plan", "--dag", dag_path, "--no-profile-feedback", "--format", "json"), extra)
        if po.stdout != ro.stdout:
            rep.bad(
                "plan-feedback:no-feedback",
                f"hint-only plan differs\n--- py ---\n{po.stdout}\n--- rs ---\n{ro.stdout}",
            )
        else:
            rep.ok("plan-feedback:no-feedback")

        # Memory-aware sizing fed by the store's rss estimates: both builds must pick the same
        # --max-steps and throttle below the CPU count (the 6 GiB heavy+solo pair overflows an
        # 8 GiB budget).
        po = run(
            py,
            ("run", "--dag", dag_path, "-q", "-k", "--max-mem", "8G", "--perf-dir", store,
             NOPROF, UNBOXED),
            extra,
        )
        ro = run(
            rs,
            ("run", "--dag", dag_path, "-q", "-k", "--max-mem", "8G", "--perf-dir", store,
             NOPROF, UNBOXED),
            extra,
        )
        ps, rss = _sizing(po.stderr), _sizing(ro.stderr)
        if ps is None or rss is None:
            rep.bad(
                "plan-feedback:sizing",
                f"missing sizing line py={po.stderr!r} rs={ro.stderr!r}",
            )
        elif ps != rss:
            rep.bad(
                "plan-feedback:sizing",
                f"(--max-steps, footprint, budget) py={ps} rs={rss}",
            )
        elif ps[0] != 1:
            rep.bad(
                "plan-feedback:sizing",
                "expected the store's rss estimates to throttle to --max-steps 1; "
                f"got --max-steps {ps[0]}",
            )
        else:
            rep.ok("plan-feedback:sizing")


# --------------------------------------------------------------------------- speedup model

#: Pinned identity for the speedup fixtures. The container class is an ``affinityN`` form so the
#: reader parses a core BUDGET (8 cores) from it — exercising the budget cap in the recommendation.
_SPEEDUP_MACHINE = "cross_speedup_machine"
_SPEEDUP_CONTAINER = "affinity8_cpu-max-max"

#: A DAG whose steps carry only est_duration HINTS; the synthetic multi-inner_jobs store below is the
#: sole source of the learned speedup curves. Four steps cover the shapes the model must distinguish.
_SPEEDUP_DAG = {
    "mem_cap_factor": 1.0,
    "mem_cap_floor_bytes": 0,
    "outer_mem_safety_factor": 1.0,
    "steps": [
        {"group": "p", "job": "lin", "desc": "linear", "cmd": "true",
         "hint": {"est_duration_s": 9.0}},
        {"group": "p", "job": "knee", "desc": "sub-linear knee", "cmd": "true",
         "hint": {"est_duration_s": 9.0}},
        {"group": "p", "job": "plat", "desc": "plateau", "cmd": "true",
         "hint": {"est_duration_s": 9.0}},
        {"group": "p", "job": "bud", "desc": "budget-capped", "cmd": "true",
         "hint": {"est_duration_s": 9.0}},
    ],
}

_SPEEDUP_HEADER = (
    "timestamp,machine_id,container_class,git_sha,outer_jobs,profile_base_sha,enforcement_kind,"
    "runner_name,step,classification,inner_jobs,elapsed_s,returncode,ok,timed_out,oom_kills,"
    "peak_bytes,thread_peak,effective_cores,user_s,sys_s,throttled_s\n"
)


def _speedup_row(
    step: str, inner_jobs: int, elapsed: str, eff: str, user_s: str, sys_s: str, throttled: str
) -> str:
    return (
        f"2026-07-26T10:00:00,{_SPEEDUP_MACHINE},{_SPEEDUP_CONTAINER},abc,1,abc,unverified,local,"
        f"{step},cpu-bound,{inner_jobs},{elapsed},0,True,False,0,1000,,{eff},{user_s},{sys_s},"
        f"{throttled}\n"
    )


def _speedup_store_csv() -> str:
    """A fixed synthetic per-step store spanning LINEAR, SUB-LINEAR (knee), PLATEAU, and
    BUDGET-CAPPED speedup shapes — the data the py and rs speedup readers must fold IDENTICALLY.

    Expected recommendations (identical in both builds):
      * p.lin   : wall 8->4->2 with flat CPU-s -> near-linear, recommend the widest measured (-j4).
      * p.knee  : wall 10->5->4.5 with CPU-s rising 10.2->18 -> sub-linear, recommend -j2 (marginal
                  gain 2->4 is <1.15 AND total CPU-s more than doubles: the knee is at 2).
      * p.plat  : wall 6->5.8 -> the -j2 gain (1.03x) is below the threshold, recommend -j1.
      * p.bud   : wall halves cleanly to -j16, but the affinity-8 core budget caps it at -j8.
    """
    rows: list[str] = []
    # p.lin — near-linear, flat total CPU-seconds (two samples per width).
    for j, walls in ((1, ("8.0", "8.0")), (2, ("4.0", "4.0")), (4, ("2.0", "2.0"))):
        for w in walls:
            rows.append(_speedup_row("p.lin", j, w, "", "8.0", "0.0", "0.0"))
    # p.knee — halves 1->2 but flattens 2->4 while total CPU-seconds blow up (work-conservation).
    rows.append(_speedup_row("p.knee", 1, "10.0", "1.0", "10.0", "0.2", "0.0"))
    rows.append(_speedup_row("p.knee", 1, "10.1", "1.0", "10.1", "0.2", "0.0"))
    rows.append(_speedup_row("p.knee", 2, "5.0", "1.98", "10.0", "0.4", "0.0"))
    rows.append(_speedup_row("p.knee", 2, "5.05", "1.98", "10.1", "0.4", "0.0"))
    rows.append(_speedup_row("p.knee", 4, "4.5", "3.2", "17.5", "0.5", "1.2"))
    rows.append(_speedup_row("p.knee", 4, "4.6", "3.2", "17.6", "0.5", "1.3"))
    # p.plat — the second width barely helps.
    rows.append(_speedup_row("p.plat", 1, "6.0", "", "6.0", "0.0", "0.0"))
    rows.append(_speedup_row("p.plat", 2, "5.8", "", "6.5", "0.0", "0.0"))
    # p.bud — perfect halving to -j16, but the core budget (8) caps the recommendation.
    for j, wall in ((1, "16.0"), (2, "8.0"), (4, "4.0"), (8, "2.0"), (16, "1.0")):
        rows.append(_speedup_row("p.bud", j, wall, "", "16.0", "0.1", "0.0"))
    return _SPEEDUP_HEADER + "".join(rows)


def _only_prefixed_file(directory: str, prefix: str, suffix: str) -> Path:
    paths = sorted(
        path
        for path in Path(directory).iterdir()
        if path.is_file() and path.name.startswith(prefix) and path.name.endswith(suffix)
    )
    if len(paths) != 1:
        raise ValueError(
            f"expected one {prefix}*{suffix} file under {directory}, found {paths}"
        )
    return paths[0]


def _write_report_capture_fixtures(store: str) -> None:
    """Add valid, malformed, and removed-step captures to a report-parity store."""

    capture = Path(store) / "captures" / "capture-001"
    artifact = capture / "perf-001" / "perf data#1.data"
    artifact.parent.mkdir(parents=True)
    artifact.write_bytes(b"PERFILE2")
    manifest: dict[str, object] = {
        "schema": "dagrun-profile-capture-v1",
        "capture_id": "capture-001",
        "state": "complete",
        "machine_id": "capture-machine",
        "container_class": "capture-container",
        "created_at": "2026-07-26T10:01:00Z",
        "finished_at": "2026-07-26T10:01:03Z",
        "artifact_root": ".",
        "selection": {
            "step": "p.lin",
            "workload_digest": "",
            "inner_jobs": 4,
            "expected_wall_s": 2.0,
            "speedup": 4.0,
            "git_sha": "abc",
        },
        "preflight": [],
        "trials": [
            {
                "trial_id": "perf-001",
                "kind": "perf",
                "state": "complete",
                "inner_jobs": 4,
                "started_at": "2026-07-26T10:01:00Z",
                "finished_at": "2026-07-26T10:01:03Z",
                "measured_wall_s": 2.1,
                "workload_returncode": 0,
                "profiler_returncode": 0,
                "included_in_model": False,
                "artifacts": [
                    {
                        "role": "perf-data",
                        "path": "perf-001/perf data#1.data",
                        "size_bytes": 8,
                        "mode": "0o600",
                    },
                    {
                        "role": "perf-log",
                        "path": "perf-001/missing.log",
                        "size_bytes": 0,
                        "mode": "0o600",
                    },
                ],
                "error": "",
            }
        ],
        "errors": [],
    }
    (capture / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")

    malformed = Path(store) / "captures" / "bad-json"
    malformed.mkdir()
    (malformed / "manifest.json").write_text("{not json", encoding="utf-8")

    legacy = Path(store) / "captures" / "legacy-capture"
    legacy.mkdir()
    legacy_manifest = dict(manifest)
    legacy_manifest.pop("machine_id")
    legacy_manifest.pop("container_class")
    legacy_manifest["capture_id"] = "legacy-capture"
    legacy_manifest["created_at"] = "2026-07-26T10:02:00Z"
    (legacy / "manifest.json").write_text(
        json.dumps(legacy_manifest), encoding="utf-8"
    )

    removed = Path(store) / "captures" / "removed-step"
    removed.mkdir()
    removed_manifest = dict(manifest)
    removed_manifest["capture_id"] = "removed-step"
    selection = manifest["selection"]
    assert isinstance(selection, dict)
    removed_manifest["selection"] = {**selection, "step": "old.removed"}
    (removed / "manifest.json").write_text(
        json.dumps(removed_manifest), encoding="utf-8"
    )


def compare_scaling_model_sidecar(py: list[str], rs: list[str], rep: Report) -> None:
    """Rebuild the saved scaling model from identical raw rows and compare its exact bytes.

    A deliberately skipped target-sweep node triggers the ordinary post-sweep refresh without
    appending a timing-dependent sample. A tiny unboxed seed run first discovers each engine's
    real machine/container filename; the generated profile is then replaced with the same fixed
    synthetic store used by :func:`compare_speedup_model`.
    """

    with tempfile.TemporaryDirectory(prefix="dagrun-scaling-model-cross-") as tmp:
        seed_dag = os.path.join(tmp, "seed.json")
        Path(seed_dag).write_text(
            '{"steps":[{"group":"seed","job":"identity","cmd":"true"}]}',
            encoding="utf-8",
        )
        stores = {name: os.path.join(tmp, name) for name in ("py", "rs")}
        commands = {"py": py, "rs": rs}
        for name, command in commands.items():
            outcome = run(
                command,
                (
                    "run",
                    "--dag",
                    seed_dag,
                    "--perf-dir",
                    stores[name],
                    NOFB,
                    "--unsafe-no-cgroups",
                    "-q",
                ),
            )
            if outcome.returncode != 0:
                rep.bad(
                    "scaling-model-sidecar",
                    f"{name} identity seed failed: {outcome}",
                )
                return
        try:
            profile_paths = {
                name: _only_prefixed_file(store, "step_profiles_", ".csv")
                for name, store in stores.items()
            }
        except (OSError, ValueError) as error:
            rep.bad("scaling-model-sidecar", str(error))
            return
        if profile_paths["py"].name != profile_paths["rs"].name:
            rep.bad(
                "scaling-model-sidecar",
                "profile identities differ before model refresh: "
                f"py={profile_paths['py'].name!r} rs={profile_paths['rs'].name!r}",
            )
            return
        for path in profile_paths.values():
            path.write_text(_speedup_store_csv(), encoding="utf-8")
        for store in stores.values():
            _write_report_capture_fixtures(store)

        raw_steps = _SPEEDUP_DAG.get("steps")
        if not isinstance(raw_steps, list):
            rep.bad("scaling-model-sidecar", "internal speedup fixture has no step list")
            return
        refresh_dag = os.path.join(tmp, "refresh.json")
        Path(refresh_dag).write_text(
            json.dumps(
                {
                    **_SPEEDUP_DAG,
                    "steps": [
                        *raw_steps,
                        {
                            "group": "refresh",
                            "job": "only",
                            "cmd": "true",
                            "skip_reason": "empty-manifest-bucket",
                        },
                    ],
                }
            ),
            encoding="utf-8",
        )
        for name, command in commands.items():
            outcome = run(
                command,
                (
                    "sweep",
                    "--dag",
                    refresh_dag,
                    "--step",
                    "refresh.only",
                    "--target-time",
                    "0",
                    "--perf-dir",
                    stores[name],
                    "--unsafe-no-cgroups",
                ),
            )
            if outcome.returncode != 0:
                rep.bad(
                    "scaling-model-sidecar",
                    f"{name} refresh failed: {outcome}",
                )
                return
        try:
            model_paths = {
                name: _only_prefixed_file(store, "scaling_model_", ".json")
                for name, store in stores.items()
            }
            encoded = {name: path.read_bytes() for name, path in model_paths.items()}
        except (OSError, ValueError) as error:
            rep.bad("scaling-model-sidecar", str(error))
            return
        if model_paths["py"].name != model_paths["rs"].name:
            rep.bad(
                "scaling-model-sidecar",
                f"model filenames differ: py={model_paths['py'].name!r} "
                f"rs={model_paths['rs'].name!r}",
            )
        elif encoded["py"] != encoded["rs"]:
            rep.bad(
                "scaling-model-sidecar",
                "model JSON is not byte-identical\n"
                f"--- py ---\n{encoded['py'].decode(errors='replace')}\n"
                f"--- rs ---\n{encoded['rs'].decode(errors='replace')}",
            )
        elif not encoded["py"].endswith(b"\n"):
            rep.bad("scaling-model-sidecar", "model JSON lacks its canonical trailing newline")
        else:
            try:
                document = json.loads(encoded["py"])
                steps = document.get("steps", []) if isinstance(document, dict) else []
                schema = document.get("schema") if isinstance(document, dict) else None
                tags = [
                    step.get("step")
                    for step in steps
                    if isinstance(step, dict) and isinstance(step.get("step"), str)
                ]
                digests = [
                    step.get("workload_digest")
                    for step in steps
                    if isinstance(step, dict)
                ]
            except (UnicodeDecodeError, json.JSONDecodeError):
                schema, tags, digests = None, [], []
            expected_tags = ["p.bud", "p.knee", "p.lin", "p.plat"]
            if schema != 2 or tags != expected_tags or len(digests) != 4 or any(
                not isinstance(digest, str) or not digest for digest in digests
            ):
                rep.bad(
                    "scaling-model-sidecar",
                    f"unexpected model schema/steps/workload digests: schema={schema!r} "
                    f"tags={tags!r} digests={digests!r}",
                )
            else:
                rep.ok("scaling-model-sidecar:byte-identical")

        report_payloads: dict[str, object] = {}
        for name, store in stores.items():
            report_path = Path(store) / "profile_report.html"
            try:
                document = report_path.read_text(encoding="utf-8")
            except OSError as error:
                rep.bad("profile-report:shared-payload", f"{name} report missing: {error}")
                return
            match = re.search(
                r'<script id="dagrun-report-data" type="application/json">(.*?)</script>',
                document,
                re.DOTALL,
            )
            if match is None:
                rep.bad(
                    "profile-report:shared-payload",
                    f"{name} report has no embedded dagrun-report-data payload",
                )
                return
            try:
                payload = json.loads(match.group(1))
            except json.JSONDecodeError as error:
                rep.bad(
                    "profile-report:shared-payload",
                    f"{name} report payload is invalid JSON: {error}",
                )
                return
            if not isinstance(payload, dict):
                rep.bad(
                    "profile-report:shared-payload",
                    f"{name} report payload is not an object",
                )
                return
            # The stores intentionally live in different temp directories; this display-only
            # field is therefore the sole expected difference for identical source data.
            payload["profile_dir"] = "<profile-store>"
            report_payloads[name] = payload
        if report_payloads.get("py") != report_payloads.get("rs"):
            rep.bad(
                "profile-report:shared-payload",
                "embedded report data differs\n"
                f"--- py ---\n{json.dumps(report_payloads.get('py'), indent=2, sort_keys=True)}\n"
                f"--- rs ---\n{json.dumps(report_payloads.get('rs'), indent=2, sort_keys=True)}",
            )
        else:
            shared = report_payloads.get("py")
            captures = shared.get("captures") if isinstance(shared, dict) else None
            environments = (
                shared.get("environments") if isinstance(shared, dict) else None
            )
            warnings = shared.get("warnings") if isinstance(shared, dict) else None
            expected_warnings = [
                "Ignored malformed capture manifest captures/bad-json/manifest.json.",
                "Ignored capture manifest captures/removed-step/manifest.json for step "
                "'old.removed', which is absent from the supplied DAG.",
            ]
            captures_by_id = (
                {
                    capture.get("capture_id"): capture
                    for capture in captures
                    if isinstance(capture, dict)
                }
                if isinstance(captures, list)
                else {}
            )
            capture = captures_by_id.get("capture-001")
            legacy = captures_by_id.get("legacy-capture")
            artifact = (
                capture["trials"][0]["artifacts"][0]
                if isinstance(capture, dict)
                else None
            )
            if (
                len(captures_by_id) != 2
                or not isinstance(capture, dict)
                or capture.get("environment")
                != "capture-machine␟capture-container"
                or not isinstance(legacy, dict)
                or legacy.get("environment") != ""
                or not isinstance(artifact, dict)
                or artifact.get("href")
                != "captures/capture-001/perf-001/perf%20data%231.data"
                or artifact.get("exists") is not True
                or not isinstance(environments, list)
                or not any(
                    isinstance(environment, dict)
                    and environment.get("key")
                    == "capture-machine␟capture-container"
                    for environment in environments
                )
                or warnings != expected_warnings
            ):
                rep.bad(
                    "profile-report:shared-payload",
                    "capture-manifest data was absent or not canonical: "
                    f"captures={captures!r} warnings={warnings!r}",
                )
                return
            rep.ok("profile-report:shared-payload")


def compare_speedup_model(py: list[str], rs: list[str], rep: Report) -> None:
    """Prove the per-step PARALLEL-SPEEDUP model is byte-identical across builds on a fixed store.

    Against a synthetic store spanning linear / sub-linear / plateau / budget-capped shapes, assert
    ``plan`` output (BOTH ``text`` and ``json``) is BYTE-IDENTICAL between the Python and Rust
    builds — so the derived speedup curves AND the recommended inner_jobs match exactly — and that
    the recommendations are the expected 4 / 2 / 1 / 8 (positive proof the model actually fired,
    not merely that both builds agree on ``null``)."""
    extra = {
        "DAGRUN_MACHINE_ID": _SPEEDUP_MACHINE,
        "DAGRUN_CONTAINER_CLASS": _SPEEDUP_CONTAINER,
    }
    with tempfile.TemporaryDirectory() as tmp:
        store = os.path.join(tmp, "store")
        os.makedirs(store, exist_ok=True)
        csv_name = f"step_profiles_{_SPEEDUP_MACHINE}_{_SPEEDUP_CONTAINER}.csv"
        with open(os.path.join(store, csv_name), "w", encoding="utf-8") as fh:
            fh.write(_speedup_store_csv())
        dag_path = os.path.join(tmp, "dag.json")
        with open(dag_path, "w", encoding="utf-8") as fh:
            fh.write(json.dumps(_SPEEDUP_DAG))

        json_out = ""
        for fmt in ("text", "json"):
            args = ("plan", "--dag", dag_path, "--perf-dir", store, "--format", fmt)
            po = run(py, args, extra)
            ro = run(rs, args, extra)
            label = f"speedup-model:{fmt}"
            if po.returncode != 0 or ro.returncode != 0:
                rep.bad(label, f"exit py={po.returncode} rs={ro.returncode} "
                               f"(py stderr={po.stderr!r}; rs stderr={ro.stderr!r})")
                continue
            if po.stdout != ro.stdout:
                rep.bad(
                    label,
                    f"speedup plan not byte-identical\n--- py ---\n{po.stdout}\n--- rs ---\n{ro.stdout}",
                )
                continue
            rep.ok(label)
            if fmt == "json":
                json_out = po.stdout

        # Positive proof: the four shapes yield the expected recommended widths in BOTH builds
        # (already asserted byte-identical above; checked once on the shared output).
        expected = {"p.lin": 4, "p.knee": 2, "p.plat": 1, "p.bud": 8}
        try:
            obj = json.loads(json_out)
            recs = {
                step["tag"]: step["speedup"]["recommended_inner_jobs"]
                for step in obj.get("steps", [])
                if isinstance(step.get("speedup"), dict)
            }
        except (json.JSONDecodeError, KeyError, TypeError):
            recs = {}
        if recs == expected:
            rep.ok("speedup-model:recommendations")
        else:
            rep.bad(
                "speedup-model:recommendations",
                f"recommended inner_jobs mismatch: expected {expected}, got {recs}",
            )

        # A run's explicit total CPU budget must also bound profile-derived recommendations shown
        # by --show-plan under EVERY planner, not only CPA. Measurements above P remain visible in
        # the curve, but no actionable recommendation may exceed the outer quota.
        for planner in ("greedy-lpt", "critical-path", "cpa"):
            run_args: tuple[str, ...] = (
                "run", "--dag", dag_path, "--perf-dir", store, "--planner", planner,
                "--show-plan", "--max-steps", "1", "--max-cpus", "2", NOPROF,
                "--unsafe-no-cgroups", "-q",
            )
            po = run(py, run_args, extra)
            ro = run(rs, run_args, extra)
            label = f"speedup-model:run-budget/{planner}"
            if po.returncode != ro.returncode or po.returncode != 0:
                rep.bad(label, f"py={po}\nrs={ro}")
                continue
            py_output = _deterministic_run_output(po.stdout)
            rs_output = _deterministic_run_output(ro.stdout)
            if py_output != rs_output:
                rep.bad(
                    label,
                    "budgeted --show-plan or semantic run output differs\n"
                    f"--- py ---\n{py_output}\n--- rs ---\n{rs_output}",
                )
                continue
            table = po.stdout.partition("parallel-speedup model")[2]
            recommendations: dict[str, int] = {}
            for step in expected:
                match = re.search(rf"(?m)^{re.escape(step)}\s+(\d+)\s+", table)
                if match is not None:
                    recommendations[step] = int(match.group(1))
            expected_capped = {"p.lin": 2, "p.knee": 2, "p.plat": 1, "p.bud": 2}
            if recommendations == expected_capped:
                rep.ok(label)
            else:
                rep.bad(
                    label,
                    f"recommendations exceed/miss P=2: expected {expected_capped}, "
                    f"got {recommendations}\n{po.stdout}",
                )

        # Positive execution proof for the profile-derived CPA width: the guest accepts exactly
        # the flag CPA should apply under P=2. This catches a plan/application disconnect even if
        # both implementations render the same recommendation table.
        observed_dag = os.path.join(tmp, "observed-width.json")
        with open(observed_dag, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "steps": [
                        {
                            "group": "p",
                            "job": "lin",
                            "cmd": 'check() { [ "$*" = "--workers=2" ]; }; check',
                            "jobs_flag": "--workers=",
                            "hint": {
                                "est_duration_s": 9.0,
                                "preferred_inner_jobs": 1,
                            },
                        }
                    ]
                },
                handle,
            )
        applied_args = (
            "run", "--dag", observed_dag, "--perf-dir", store, "--planner", "cpa",
            "--max-steps", "1", "--max-cpus", "2", NOPROF, "--unsafe-no-cgroups", "-q",
        )
        po = run(py, applied_args, extra)
        ro = run(rs, applied_args, extra)
        if po.returncode == ro.returncode == 0:
            rep.ok("speedup-model:cpa-applied-width")
        else:
            rep.bad(
                "speedup-model:cpa-applied-width",
                f"profile-derived CPA width did not reach the guest exactly: py={po}\nrs={ro}",
            )


# --------------------------------------------------------------------------- CPA planner

#: Pinned identity for the CPA fixtures. An ``affinityN`` container so the speedup reader parses a
#: 16-core knee budget (well above the widths used), keeping the recommended widths curve-limited
#: rather than budget-limited.
_CPA_MACHINE = "cross_cpa_machine"
_CPA_CONTAINER = "affinity16_cpu-max-max"

#: A DAG with a long dependency chain (prep -> build -> test) plus an independent plateau step
#: (side). All durations come from the synthetic store below. Its shape makes CPA earn its keep: at
#: inner-jobs=1 the chain critical path is prep(2)+build(40)+test(16) = 58s; the allocator widens the
#: two SCALING chain steps (build, test) to shrink that path while the plateau step (side) and the
#: curveless prep stay narrow.
_CPA_DAG = {
    "mem_cap_factor": 1.0,
    "mem_cap_floor_bytes": 0,
    "outer_mem_safety_factor": 1.0,
    "steps": [
        {"group": "c", "job": "prep", "desc": "prep", "cmd": "true",
         "hint": {"est_duration_s": 2.0}},
        {"group": "c", "job": "build", "desc": "build", "cmd": "true", "deps": ["c.prep"],
         "hint": {"est_duration_s": 40.0}},
        {"group": "c", "job": "test", "desc": "test", "cmd": "true", "deps": ["c.build"],
         "hint": {"est_duration_s": 16.0}},
        {"group": "c", "job": "side", "desc": "side", "cmd": "true",
         "hint": {"est_duration_s": 9.0}},
    ],
}

#: The width-1 critical-path length of :data:`_CPA_DAG` (prep 2 + build 40 + test 16). This is the
#: makespan of the "critical-path with fixed inner-jobs = 1" baseline (side runs in parallel), which
#: the CPA allocation must BEAT once it can widen the scaling chain steps (any core budget >= 2).
_CPA_WIDTH1_CRITICAL_PATH_S = 58.0

_CPA_HEADER = (
    "timestamp,machine_id,container_class,git_sha,outer_jobs,profile_base_sha,enforcement_kind,"
    "runner_name,step,classification,inner_jobs,elapsed_s,returncode,ok,timed_out,oom_kills,"
    "peak_bytes,thread_peak,effective_cores,user_s,sys_s,throttled_s\n"
)


def _cpa_row(
    machine: str, container: str, step: str, inner_jobs: int, elapsed: str, user_s: str,
    peak_bytes: str = "1000",
) -> str:
    return (
        f"t,{machine},{container},a,1,a,unverified,local,{step},cpu-bound,{inner_jobs},{elapsed},"
        f"0,True,False,0,{peak_bytes},,,{user_s},0.0,0.0\n"
    )


def _cpa_store_csv() -> str:
    """A fixed synthetic store for :data:`_CPA_DAG`: build and test scale near-linearly (flat total
    CPU-seconds, so the work-conservation knee does not truncate them) while side plateaus after
    inner-jobs=1. So the allocator's admissible widths are build up to 8, test up to 4, side rigid at
    1 — and CPA widens build+test to collapse the 58s chain."""
    rows: list[str] = []
    for j, w in ((1, "40.0"), (2, "20.0"), (4, "10.0"), (8, "5.0")):
        rows.append(_cpa_row(_CPA_MACHINE, _CPA_CONTAINER, "c.build", j, w, "40.0"))
    for j, w in ((1, "16.0"), (2, "8.0"), (4, "4.0")):
        rows.append(_cpa_row(_CPA_MACHINE, _CPA_CONTAINER, "c.test", j, w, "16.0"))
    for j, w in ((1, "9.0"), (2, "8.7")):
        rows.append(_cpa_row(_CPA_MACHINE, _CPA_CONTAINER, "c.side", j, w, "9.0"))
    return _CPA_HEADER + "".join(rows)


#: A DAG whose one scaling CPU-bound step carries a memory baseline (3 GiB, factor 1.0) so its cap
#: SCALES with width above inner-jobs=4 (``step_mem_cap_for_inner_jobs``: cap*p/4). Widening it
#: 4 -> 8 doubles the modeled footprint from 3 GiB to 6 GiB, so a 5G RAM budget must BLOCK that
#: widening (mem-capped) even though cores are free.
_CPA_MEM_DAG = {
    "mem_cap_factor": 1.0,
    "mem_cap_floor_bytes": 0,
    "outer_mem_safety_factor": 1.0,
    "steps": [
        {"group": "m", "job": "prep", "desc": "prep", "cmd": "true",
         "hint": {"est_duration_s": 2.0}},
        {"group": "m", "job": "heavy", "desc": "heavy", "cmd": "true", "deps": ["m.prep"],
         "hint": {"est_duration_s": 40.0, "rss_baseline_bytes": 3221225472,
                  "classification": "cpu-bound"}},
    ],
}


def _cpa_mem_store_csv() -> str:
    """Store for :data:`_CPA_MEM_DAG`: m.heavy scales near-linearly to inner-jobs=8 (flat CPU-s).

    Every row also carries the same measured 3-GiB RSS as the DAG hint. CPA correctly plans from
    the selected store estimate, so the fixture must not depend on the pre-fix bug where allocation
    saw the authored 3-GiB hint but execution later installed a tiny learned RSS value.
    """
    rows = [
        _cpa_row(
            _CPA_MACHINE,
            _CPA_CONTAINER,
            "m.heavy",
            j,
            w,
            "40.0",
            peak_bytes="3221225472",
        )
        for j, w in ((1, "40.0"), (2, "20.0"), (4, "10.0"), (8, "5.0"))
    ]
    return _CPA_HEADER + "".join(rows)


def _cpa_widths_from_json(stdout: str) -> dict[str, int] | None:
    """Extract ``{tag: alloc_inner_jobs}`` from ``plan --planner cpa --format json`` output."""
    try:
        obj = json.loads(stdout)
    except json.JSONDecodeError:
        return None
    if not isinstance(obj, dict):
        return None
    steps = obj.get("steps")
    if not isinstance(steps, list):
        return None
    out: dict[str, int] = {}
    for s in steps:
        if not isinstance(s, dict):
            return None
        tag, w = s.get("tag"), s.get("alloc_inner_jobs")
        if isinstance(tag, str) and isinstance(w, int):
            out[tag] = w
    return out


def compare_cpa_planner(py: list[str], rs: list[str], rep: Report) -> None:
    """Prove the ``--planner cpa`` moldable allocator agrees across builds AND actually helps.

    Against a fixed synthetic store (host-independent identity), for both output formats:

    * ``plan --planner cpa`` output is BYTE-IDENTICAL py vs rs (so the chosen widths, the dispatch
      order at the allocated weights, the allocator summary, the makespan lower bound, and the
      modeled makespan all match exactly).
    * the allocator FIRED — it widened the scaling chain step (``c.build`` gets inner-jobs > 1) —
      and the modeled makespan never dips below the ``max(T_CP, area/P)`` lower bound.
    * CPA BEATS the fixed-inner-jobs=1 baseline: its modeled makespan is below the 58s width-1
      critical path (whenever the core budget allows any widening, i.e. P >= 2).
    * the MEMORY cap binds: with a tight ``--max-mem`` the memory-heavy step is widened LESS than
      with no budget (and the run reports ``mem-capped``) — provided cores were not the limit.

    ``P`` is the ambient core budget, the tighter of the CPU affinity mask and any cgroup CPU
    quota. Below two cores the widening and beats-fixed checks are skipped as ``multi-cpu``;
    below eight the memory-cap check is skipped as ``eight-cpu``. None of them counts as a pass.
    """
    extra = {
        "DAGRUN_MACHINE_ID": _CPA_MACHINE,
        "DAGRUN_CONTAINER_CLASS": _CPA_CONTAINER,
    }
    with tempfile.TemporaryDirectory() as tmp:
        store = os.path.join(tmp, "store")
        os.makedirs(store, exist_ok=True)
        csv_name = f"step_profiles_{_CPA_MACHINE}_{_CPA_CONTAINER}.csv"
        with open(os.path.join(store, csv_name), "w", encoding="utf-8") as fh:
            fh.write(_cpa_store_csv())
        dag_path = os.path.join(tmp, "dag.json")
        with open(dag_path, "w", encoding="utf-8") as fh:
            fh.write(json.dumps(_CPA_DAG))

        json_out = ""
        for fmt in ("text", "json"):
            args = ("plan", "--dag", dag_path, "--perf-dir", store, "--planner", "cpa",
                    "--format", fmt)
            po = run(py, args, extra)
            ro = run(rs, args, extra)
            label = f"cpa:{fmt}"
            if po.returncode != 0 or ro.returncode != 0:
                rep.bad(label, f"exit py={po.returncode} rs={ro.returncode} "
                               f"(py stderr={po.stderr!r}; rs stderr={ro.stderr!r})")
                continue
            if po.stdout != ro.stdout:
                rep.bad(label, f"cpa plan not byte-identical\n--- py ---\n{po.stdout}\n"
                               f"--- rs ---\n{ro.stdout}")
                continue
            rep.ok(label)
            if fmt == "json":
                json_out = po.stdout

        # Positive proofs on the shared (byte-identical) JSON.
        try:
            obj = json.loads(json_out)
            alloc = obj["allocation"]
            widths = _cpa_widths_from_json(json_out) or {}
            core_budget = int(alloc["core_budget"])
            modeled = float(alloc["modeled_makespan_s"])
            lower_bound = float(alloc["lower_bound_s"])
        except (json.JSONDecodeError, KeyError, TypeError, ValueError):
            rep.bad("cpa:allocation", f"unparseable allocation in\n{json_out}")
            return

        # Standalone CPA plans for the ambient core budget (the tighter of the CPU affinity mask
        # and any cgroup CPU quota). Below two cores no step can be widened, so the two checks
        # that need a widening did not run: they are skips, never passes.
        cannot_widen = (
            f"the core budget is {core_budget}; a plan can widen a step only with at least two"
        )

        # The allocator actually allocated: the scaling chain head widened past 1 (needs P >= 2).
        if core_budget < 2:
            rep.skip("cpa:widened", "multi-cpu", cannot_widen)
        elif widths.get("c.build", 0) > 1:
            rep.ok("cpa:widened")
        else:
            rep.bad("cpa:widened",
                    f"expected c.build to be widened past 1 (P={core_budget}); widths={widths}")

        # The modeled makespan must never dip below the max(T_CP, area/P) lower bound.
        if modeled + 1e-9 >= lower_bound:
            rep.ok("cpa:modeled-ge-lower-bound")
        else:
            rep.bad("cpa:modeled-ge-lower-bound",
                    f"modeled {modeled} < lower_bound {lower_bound}")

        # CPA beats the fixed-inner-jobs=1 baseline (58s width-1 critical path) once it can widen.
        if core_budget < 2:
            rep.skip("cpa:beats-fixed", "multi-cpu", cannot_widen)
        elif modeled < _CPA_WIDTH1_CRITICAL_PATH_S:
            rep.ok("cpa:beats-fixed")
        else:
            rep.bad("cpa:beats-fixed",
                    f"CPA modeled makespan {modeled}s did not beat the width-1 critical path "
                    f"{_CPA_WIDTH1_CRITICAL_PATH_S}s (P={core_budget})")

        # Memory cap: a tight --max-mem must widen the memory-heavy step LESS than an unbounded run.
        mstore = os.path.join(tmp, "mstore")
        os.makedirs(mstore, exist_ok=True)
        with open(os.path.join(mstore, csv_name), "w", encoding="utf-8") as fh:
            fh.write(_cpa_mem_store_csv())
        mdag = os.path.join(tmp, "mdag.json")
        with open(mdag, "w", encoding="utf-8") as fh:
            fh.write(json.dumps(_CPA_MEM_DAG))
        free_args = ("plan", "--dag", mdag, "--perf-dir", mstore, "--planner", "cpa",
                     "--format", "json")
        capped_args = free_args + ("--max-mem", "5G")
        pf, rf = run(py, free_args, extra), run(rs, free_args, extra)
        pc, rc = run(py, capped_args, extra), run(rs, capped_args, extra)
        if pf.stdout != rf.stdout or pc.stdout != rc.stdout:
            rep.bad("cpa:mem-byte-identical",
                    "cpa --max-mem plan not byte-identical py vs rs")
        else:
            rep.ok("cpa:mem-byte-identical")
            free_w = (_cpa_widths_from_json(pf.stdout) or {}).get("m.heavy", 0)
            capped_w = (_cpa_widths_from_json(pc.stdout) or {}).get("m.heavy", 0)
            capped_budget: int | None
            try:
                capped_reason = json.loads(pc.stdout)["allocation"]["stop_reason"]
                capped_budget = int(json.loads(pc.stdout)["allocation"]["core_budget"])
            except (json.JSONDecodeError, KeyError, TypeError, ValueError):
                capped_reason, capped_budget = "", None
            # The memory throttle can be observed only when cores were not the binding constraint
            # (P >= 8, so the unbounded run could actually reach width 8 where the footprint
            # doubles). Below that the assertion did not run, so it is a skip, never a pass. An
            # allocation that cannot be read is a failure: it says nothing about the core budget.
            if capped_budget is None:
                rep.bad("cpa:mem-capped",
                        f"unparseable --max-mem allocation (py exit {pc.returncode}, rs exit "
                        f"{rc.returncode}) in\n{pc.stdout}")
            elif capped_budget < 8:
                rep.skip(
                    "cpa:mem-capped",
                    "eight-cpu",
                    f"the core budget is {capped_budget}; the 5G memory cap can bind only when "
                    "the unbounded plan could widen m.heavy to 8",
                )
            elif capped_w < free_w and capped_reason == "mem-capped":
                rep.ok("cpa:mem-capped")
            else:
                rep.bad("cpa:mem-capped",
                        f"expected --max-mem to throttle m.heavy below the unbounded width "
                        f"{free_w} with stop_reason 'mem-capped'; got width {capped_w} "
                        f"reason {capped_reason!r} (P={capped_budget})")

        # A self-managed command wider than any realistic ambient P is not moldable. Standalone
        # CPA must preserve that declared width as infeasible, not invent P as a guest width; the
        # two renderers must agree on the null allocation and infinite modeled makespan.
        fixed_dag = os.path.join(tmp, "fixed-width.json")
        with open(fixed_dag, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "steps": [
                        {
                            "group": "f",
                            "job": "fixed",
                            "cmd": "true",
                            "jobs_flag": "",
                            "hint": {
                                "est_duration_s": 1.0,
                                "preferred_inner_jobs": 1_000_000_000,
                            },
                        }
                    ]
                },
                handle,
            )
        fixed_args = (
            "plan", "--dag", fixed_dag, "--planner", "cpa", "--format", "json",
            "--no-profile-feedback",
        )
        pfixed = run(py, fixed_args)
        rfixed = run(rs, fixed_args)
        fixed_ok = False
        if pfixed.returncode == rfixed.returncode == 0 and pfixed.stdout == rfixed.stdout:
            try:
                fixed_obj = json.loads(pfixed.stdout)
                fixed_alloc = fixed_obj["allocation"]
                fixed_step = fixed_obj["steps"][0]
                fixed_ok = (
                    fixed_alloc["stop_reason"] == "infeasible-fixed-width"
                    and fixed_alloc["modeled_makespan_s"] == "inf"
                    and fixed_step["alloc_inner_jobs"] is None
                )
            except (json.JSONDecodeError, KeyError, IndexError, TypeError):
                fixed_ok = False
        if fixed_ok:
            rep.ok("cpa:infeasible-fixed-width")
        else:
            rep.bad(
                "cpa:infeasible-fixed-width",
                "fixed self-managed width was not rendered as the same infeasible CPA plan\n"
                f"py={pfixed}\nrs={rfixed}",
            )

        # Intentional skips have zero executable demand. Adding a huge skipped node must not
        # suppress the live step's width, and both plans must expose the skip as zero/skip/null.
        control_dag = os.path.join(tmp, "skip-control.json")
        skipped_dag = os.path.join(tmp, "skip-present.json")
        live_step = {
            "group": "c",
            "job": "build",
            "cmd": "true",
            "jobs_flag": "-j%d",
            "hint": {"est_duration_s": 40.0, "preferred_inner_jobs": 1},
        }
        with open(control_dag, "w", encoding="utf-8") as handle:
            json.dump({"steps": [live_step]}, handle)
        with open(skipped_dag, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "steps": [
                        {
                            "group": "c",
                            "job": "skipped",
                            "cmd": "false",
                            "jobs_flag": "",
                            "skip_reason": "empty-manifest-bucket",
                            "hint": {
                                "est_duration_s": 100.0,
                                "rss_baseline_bytes": 1_000_000_000_000,
                                "preferred_inner_jobs": 1_000_000_000,
                            },
                        },
                        live_step,
                    ]
                },
                handle,
            )
        control_args = (
            "plan", "--dag", control_dag, "--perf-dir", store, "--planner", "cpa",
            "--format", "json",
        )
        skipped_args = (
            "plan", "--dag", skipped_dag, "--perf-dir", store, "--planner", "cpa",
            "--format", "json",
        )
        pcontrol, rcontrol = run(py, control_args, extra), run(rs, control_args, extra)
        pskipped, rskipped = run(py, skipped_args, extra), run(rs, skipped_args, extra)
        skip_ok = False
        if pcontrol.stdout == rcontrol.stdout and pskipped.stdout == rskipped.stdout:
            try:
                control_obj = json.loads(pcontrol.stdout)
                skipped_obj = json.loads(pskipped.stdout)
                control_live = next(
                    step for step in control_obj["steps"] if step["tag"] == "c.build"
                )
                skipped_by_tag = {step["tag"]: step for step in skipped_obj["steps"]}
                skip_entry = skipped_by_tag["c.skipped"]
                skip_ok = (
                    skipped_by_tag["c.build"]["alloc_inner_jobs"]
                    == control_live["alloc_inner_jobs"]
                    and skip_entry["est_duration_s"] == "0.000"
                    and skip_entry["est_source"] == "skip"
                    and skip_entry["alloc_inner_jobs"] is None
                )
            except (json.JSONDecodeError, KeyError, StopIteration, TypeError):
                skip_ok = False
        if skip_ok:
            rep.ok("cpa:intentional-skip-zero-demand")
        else:
            rep.bad(
                "cpa:intentional-skip-zero-demand",
                "intentional skip changed live allocation or was not zero/skip/null\n"
                f"control py={pcontrol}\ncontrol rs={rcontrol}\n"
                f"skip py={pskipped}\nskip rs={rskipped}",
            )

        # A self-managed fixed width uses an exact measured curve level when one exists. With no
        # exact level it falls back to the independently resolved scalar estimate (store-derived in
        # this CLI fixture); it must not substitute a neighboring curve width.
        fixed_curve_dag = os.path.join(tmp, "fixed-curve-source.json")
        with open(fixed_curve_dag, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "steps": [
                        {
                            "group": "c",
                            "job": "build",
                            "cmd": "true",
                            "jobs_flag": "",
                            "hint": {"est_duration_s": 13.0, "preferred_inner_jobs": 2},
                        },
                        {
                            "group": "c",
                            "job": "test",
                            "cmd": "true",
                            "jobs_flag": "",
                            "hint": {"est_duration_s": 13.0, "preferred_inner_jobs": 3},
                        },
                    ]
                },
                handle,
            )
        source_args = (
            "plan", "--dag", fixed_curve_dag, "--perf-dir", store, "--planner", "cpa",
            "--format", "json",
        )
        psource, rsource = run(py, source_args, extra), run(rs, source_args, extra)
        source_ok = False
        if psource.returncode == rsource.returncode == 0 and psource.stdout == rsource.stdout:
            try:
                source_obj = json.loads(psource.stdout)
                source_by_tag = {step["tag"]: step for step in source_obj["steps"]}
                source_ok = (
                    source_by_tag["c.build"]["est_duration_s"] == "20.000"
                    and source_by_tag["c.build"]["est_source"] == "store"
                    and source_by_tag["c.test"]["est_duration_s"] == "8.000"
                    and source_by_tag["c.test"]["est_source"] == "store"
                    and source_by_tag["c.build"]["alloc_inner_jobs"] is None
                    and source_by_tag["c.test"]["alloc_inner_jobs"] is None
                )
            except (json.JSONDecodeError, KeyError, TypeError):
                source_ok = False
        if source_ok:
            rep.ok("cpa:self-managed-curve-source")
        else:
            rep.bad(
                "cpa:self-managed-curve-source",
                "self-managed exact/scalar curve provenance diverged or was mislabeled\n"
                f"py={psource}\nrs={rsource}",
            )


def compare_memory_hardening(py: list[str], rs: list[str], rep: Report) -> None:
    """Cross-check the memory semantics that must agree before scheduling begins.

    These cases are intentionally CLI-level rather than duplicate unit tests. They prove that the
    two independently packaged implementations make the same externally visible decision after
    config loading, profile enrichment, CPA allocation, and ``--max-mem`` sizing:

    * CPU-bound caps use each step's effective width;
    * hard-cap-only, default-capped, and selected ``engine_only`` steps remain real memory demand;
    * an infeasible one-step budget refuses before the guest can spawn;
    * learned RSS can make a CPA plan explicitly ``infeasible-memory``; and
    * large concurrent sums saturate in the shared signed-64 domain rather than wrapping or using
      Python's unbounded integer as a different model.
    """

    gib = 1024**3
    i64_max = 2**63 - 1

    def sizing_case(
        directory: str,
        label: str,
        dag: Mapping[str, object],
        budget: str,
        max_cpus: int,
        expected: tuple[int, int, int],
    ) -> None:
        dag_path = os.path.join(directory, f"{label}.json")
        with open(dag_path, "w", encoding="utf-8") as handle:
            json.dump(dag, handle)
        args = (
            "run", "--dag", dag_path, "--max-mem", budget, "--max-cpus", str(max_cpus),
            "--unsafe-no-cgroups", "-q", NOPROF, NOFB,
        )
        po, ro = run(py, args), run(rs, args)
        py_sizing, rs_sizing = _sizing(po.stderr), _sizing(ro.stderr)
        if po.returncode != ro.returncode or po.returncode != 0:
            rep.bad(f"memory:{label}", f"py={po}\nrs={ro}")
        elif py_sizing is None or rs_sizing is None:
            rep.bad(
                f"memory:{label}",
                f"missing sizing evidence py={po.stderr!r} rs={ro.stderr!r}",
            )
        elif py_sizing != rs_sizing:
            rep.bad(
                f"memory:{label}",
                f"sizing differs py={py_sizing} rs={rs_sizing}",
            )
        elif py_sizing != expected:
            rep.bad(
                f"memory:{label}",
                f"expected sizing {expected}, got {py_sizing}",
            )
        else:
            rep.ok(f"memory:{label}")

    def refusal_case(
        directory: str,
        label: str,
        step: Mapping[str, object],
        *,
        budget: str,
        budget_bytes: int,
        footprint: int,
    ) -> None:
        marker = {
            engine: os.path.join(directory, f"{label}-{engine}-spawned")
            for engine in ("py", "rs")
        }
        value: dict[str, object] = {
            "mem_cap_factor": 1.0,
            "mem_cap_floor_bytes": 0,
            "outer_mem_safety_factor": 1.0,
            "steps": [step],
        }
        dag_path = os.path.join(directory, f"{label}.json")
        with open(dag_path, "w", encoding="utf-8") as handle:
            json.dump(value, handle)
        args = (
            "run", "--dag", dag_path, "--max-mem", budget, "--max-cpus", "2",
            "--unsafe-no-cgroups", "-q", NOPROF, NOFB,
        )
        outcomes = {
            engine: run(command, args, {"MEMORY_MARKER": marker[engine]})
            for engine, command in (("py", py), ("rs", rs))
        }
        phrase = (
            f"minimum runnable footprint {footprint} bytes cannot fit safely within budget "
            f"{budget_bytes} bytes"
        )
        if (
            all(outcome.returncode == 2 for outcome in outcomes.values())
            and all(phrase in outcome.stderr for outcome in outcomes.values())
            and not any(os.path.exists(path) for path in marker.values())
        ):
            rep.ok(f"memory:{label}")
        else:
            rep.bad(
                f"memory:{label}",
                f"expected pre-spawn infeasible refusal containing {phrase!r}; "
                f"outcomes={outcomes} markers={marker}",
            )

    with tempfile.TemporaryDirectory(prefix="dagrun-cross-memory-") as tmp:
        width_scaled = {
            "mem_cap_factor": 1.0,
            "mem_cap_floor_bytes": 0,
            "outer_mem_safety_factor": 1.0,
            "steps": [
                {
                    "group": "width",
                    "job": "preferred",
                    "cmd": "sleep 0.05",
                    "jobs_flag": "",
                    "hint": {
                        "rss_baseline_bytes": gib,
                        "classification": "cpu-bound",
                        "preferred_inner_jobs": 8,
                    },
                },
                {
                    "group": "width",
                    "job": "sibling",
                    "cmd": "sleep 0.05",
                    "jobs_flag": "",
                    "hint": {
                        "rss_baseline_bytes": gib,
                        "classification": "cpu-bound",
                        "preferred_inner_jobs": 8,
                    },
                },
            ],
        }
        sizing_case(
            tmp,
            "effective-j8",
            width_scaled,
            "3G",
            8,
            (1, 2 * gib, 3 * gib),
        )

        # A nonbinding memory model must never loosen the CPU-derived active-step base. Four
        # independent 1-GiB steps fit the 16-GiB memory budget, so the modeled ceiling reaches the
        # host CPU count; explicit -j1 still makes the final active-step ceiling exactly one. A
        # genuinely one-CPU host cannot demonstrate a strictly looser memory ceiling and is
        # reported as a capability skip after all remaining invariants agree.
        nonbinding = {
            "mem_cap_factor": 1.0,
            "mem_cap_floor_bytes": 0,
            "outer_mem_safety_factor": 1.0,
            "steps": [
                {
                    "group": "nonbinding",
                    "job": f"s{index}",
                    "cmd": "sleep 0.05",
                    "jobs_flag": "",
                    "hint": {"hard_mem_max_bytes": gib},
                }
                for index in range(4)
            ],
        }
        nonbinding_path = os.path.join(tmp, "nonbinding-max-mem.json")
        with open(nonbinding_path, "w", encoding="utf-8") as handle:
            json.dump(nonbinding, handle)
        nonbinding_args = (
            "run", "--dag", nonbinding_path, "--max-mem", "16G", "--max-cpus", "1",
            "--unsafe-no-cgroups", "-q", NOPROF, NOFB,
        )
        pnon, rnon = run(py, nonbinding_args), run(rs, nonbinding_args)
        pdetails, rdetails = _sizing_details(pnon.stderr), _sizing_details(rnon.stderr)
        common_nonbinding = (
            pnon.returncode == rnon.returncode == 0
            and pdetails is not None
            and pdetails == rdetails
            and pdetails[1] == min(pdetails[0], 4) * gib
            and pdetails[2:] == (16 * gib, 1, 1)
        )
        if common_nonbinding and pdetails is not None and pdetails[0] > 1:
            rep.ok("memory:nonbinding-max-mem-keeps-cpu-base")
        elif common_nonbinding and pdetails is not None and pdetails[0] == 1:
            print(
                "cross[dagrun]: SKIP strict nonbinding max-mem proof: "
                "host exposes only one online CPU"
            )
            rep.skip(
                "memory:nonbinding-max-mem-keeps-cpu-base",
                "multi-cpu",
                "host exposes only one online CPU",
            )
        else:
            rep.bad(
                "memory:nonbinding-max-mem-keeps-cpu-base",
                f"expected memory ceiling >1 (or one-CPU capability) but final S=1; "
                f"py={pnon} details={pdetails} rs={rnon} details={rdetails}",
            )

        # Six-exabyte caps overflow an i64 when two siblings co-run. The modeled peak must
        # saturate to the i64::MAX unbounded sentinel, which fits no finite budget (including MAX),
        # so sizing safely falls back to one finite-width step. Wrapping could admit both.
        huge = 6_000_000_000_000_000_000
        saturated = {
            "mem_cap_factor": 1.0,
            "mem_cap_floor_bytes": 0,
            "outer_mem_safety_factor": 1.0,
            "steps": [
                {
                    "group": "huge",
                    "job": f"s{index}",
                    "cmd": "true",
                    "jobs_flag": "",
                    "hint": {"hard_mem_max_bytes": huge},
                }
                for index in range(2)
            ],
        }
        sizing_case(
            tmp,
            "saturating-sum",
            saturated,
            str(i64_max),
            2,
            (1, huge, i64_max),
        )

        marker_step = {
            "group": "memory",
            "job": "probe",
            "cmd": 'printf spawned > "$MEMORY_MARKER"',
            "jobs_flag": "",
        }
        refusal_case(
            tmp,
            "hard-cap-only-infeasible",
            {
                **marker_step,
                "hint": {"hard_mem_max_bytes": 3 * gib},
            },
            budget="2G",
            budget_bytes=2 * gib,
            footprint=3 * gib,
        )
        refusal_case(
            tmp,
            "default-cap-infeasible",
            marker_step,
            budget="512M",
            budget_bytes=gib // 2,
            footprint=gib,
        )
        refusal_case(
            tmp,
            "engine-only-infeasible",
            {
                **marker_step,
                "engine_only": True,
                "hint": {"hard_mem_max_bytes": 3 * gib},
            },
            budget="2G",
            budget_bytes=2 * gib,
            footprint=3 * gib,
        )
        refusal_case(
            tmp,
            "unbounded-sentinel-infeasible",
            {
                **marker_step,
                "hint": {"hard_mem_max_bytes": i64_max},
            },
            budget=str(i64_max),
            budget_bytes=i64_max,
            footprint=i64_max,
        )

        store = os.path.join(tmp, "store")
        os.makedirs(store, exist_ok=True)
        csv_name = f"step_profiles_{_CPA_MACHINE}_{_CPA_CONTAINER}.csv"
        with open(os.path.join(store, csv_name), "w", encoding="utf-8") as handle:
            handle.write(
                _CPA_HEADER
                + _cpa_row(
                    _CPA_MACHINE,
                    _CPA_CONTAINER,
                    "learn.heavy",
                    1,
                    "10.0",
                    "10.0",
                    str(5 * gib),
                )
                + "".join(
                    _cpa_row(
                        _CPA_MACHINE,
                        _CPA_CONTAINER,
                        "floor.quiet",
                        width,
                        "3.0" if width == 1 else "2.0",
                        "3.0",
                        str(gib),
                    )
                    for width in (1, 2)
                    for _ in range(3)
                )
            )
        floor_dag = os.path.join(tmp, "authored-rss-floor.json")
        with open(floor_dag, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "steps": [
                        {
                            "group": "floor",
                            "job": "quiet",
                            "cmd": "true",
                            "hint": {
                                "est_duration_s": 10.0,
                                "rss_baseline_bytes": 8 * gib,
                            },
                        }
                    ]
                },
                handle,
            )
        extra = {
            "DAGRUN_MACHINE_ID": _CPA_MACHINE,
            "DAGRUN_CONTAINER_CLASS": _CPA_CONTAINER,
        }
        floor_args = ("plan", "--dag", floor_dag, "--perf-dir", store, "--format", "json")
        pfloor, rfloor = run(py, floor_args, extra), run(rs, floor_args, extra)
        floor_ok = False
        if pfloor.returncode == rfloor.returncode == 0 and pfloor.stdout == rfloor.stdout:
            try:
                floor_step = json.loads(pfloor.stdout)["steps"][0]
                floor_ok = (
                    floor_step["est_duration_s"] == "3.000"
                    and floor_step["est_source"] == "store"
                    and floor_step["rss_estimate_bytes"] == 8 * gib
                    and floor_step["rss_source"] == "hint"
                )
            except (json.JSONDecodeError, KeyError, IndexError, TypeError):
                floor_ok = False
        if floor_ok:
            rep.ok("memory:ordinary-feedback-keeps-authored-rss-floor")
        else:
            rep.bad(
                "memory:ordinary-feedback-keeps-authored-rss-floor",
                f"one quiet sample lowered the authored RSS floor or engines diverged\n"
                f"py={pfloor}\nrs={rfloor}",
            )

        learned_dag = os.path.join(tmp, "learned-rss.json")
        learned_marker = {
            engine: os.path.join(tmp, f"learned-{engine}-spawned")
            for engine in ("py", "rs")
        }
        with open(learned_dag, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "mem_cap_factor": 1.0,
                    "mem_cap_floor_bytes": 0,
                    "outer_mem_safety_factor": 1.0,
                    "steps": [
                        {
                            "group": "learn",
                            "job": "heavy",
                            "cmd": 'printf spawned > "$MEMORY_MARKER"',
                            "hint": {
                                "est_duration_s": 10.0,
                                "classification": "cpu-bound",
                                "preferred_inner_jobs": 1,
                            },
                        }
                    ],
                },
                handle,
            )
        plan_args = (
            "plan", "--dag", learned_dag, "--perf-dir", store, "--planner", "cpa",
            "--max-mem", "4G", "--format", "json",
        )
        pplan, rplan = run(py, plan_args, extra), run(rs, plan_args, extra)
        learned_ok = False
        if pplan.returncode == rplan.returncode == 0 and pplan.stdout == rplan.stdout:
            try:
                obj = json.loads(pplan.stdout)
                step = obj["steps"][0]
                allocation = obj["allocation"]
                learned_ok = (
                    step["rss_estimate_bytes"] == 5 * gib
                    and step["rss_source"] == "store"
                    and step["alloc_inner_jobs"] is None
                    and allocation["stop_reason"] == "infeasible-memory"
                    and allocation["modeled_makespan_s"] == "inf"
                )
            except (json.JSONDecodeError, KeyError, IndexError, TypeError):
                learned_ok = False
        if learned_ok:
            rep.ok("memory:cpa-learned-rss-infeasible-plan")
        else:
            rep.bad(
                "memory:cpa-learned-rss-infeasible-plan",
                f"learned RSS did not produce one byte-identical infeasible plan\n"
                f"py={pplan}\nrs={rplan}",
            )

        run_args = (
            "run", "--dag", learned_dag, "--perf-dir", store, "--planner", "cpa",
            "--max-mem", "4G", "--max-cpus", "8", "--unsafe-no-cgroups", "-q", NOPROF,
        )
        outcomes = {
            engine: run(
                command,
                run_args,
                {**extra, "MEMORY_MARKER": learned_marker[engine]},
            )
            for engine, command in (("py", py), ("rs", rs))
        }
        phrase = "CPA allocation is infeasible under --max-mem 4G"
        if (
            all(outcome.returncode == 2 for outcome in outcomes.values())
            and all(phrase in outcome.stderr for outcome in outcomes.values())
            and not any(os.path.exists(path) for path in learned_marker.values())
        ):
            rep.ok("memory:cpa-learned-rss-run-refusal")
        else:
            rep.bad(
                "memory:cpa-learned-rss-run-refusal",
                f"expected learned-RSS CPA refusal before spawn; "
                f"outcomes={outcomes} markers={learned_marker}",
            )

        # Stress performs an authored preflight before tag expansion, then applies learned RSS to
        # the expanded tags and runs one final no-spawn footprint guard over that planned graph.
        # The final guard must neither miss the `#N` profile rows nor multiply the expanded graph
        # by N a second time.
        stress_store = os.path.join(tmp, "stress-store")
        os.makedirs(stress_store, exist_ok=True)
        stress_csv = f"step_profiles_{_CPA_MACHINE}_{_CPA_CONTAINER}.csv"
        learned_rss = 4_000_000_000_000_000_000
        with open(os.path.join(stress_store, stress_csv), "w", encoding="utf-8") as handle:
            handle.write(
                _CPA_HEADER
                + "".join(
                    _cpa_row(
                        _CPA_MACHINE,
                        _CPA_CONTAINER,
                        f"stress.seeded#{copy}",
                        1,
                        "1.0",
                        "0.1",
                        str(learned_rss),
                    )
                    for copy in (1, 2)
                )
            )
        stress_dag = os.path.join(tmp, "seeded-profile-stress.json")
        stress_marker = {
            engine: os.path.join(tmp, f"stress-profile-{engine}-spawned")
            for engine in ("py", "rs")
        }
        with open(stress_dag, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "mem_cap_factor": 1.0,
                    "mem_cap_floor_bytes": 0,
                    "outer_mem_safety_factor": 1.0,
                    "steps": [
                        {
                            "group": "stress",
                            "job": "seeded",
                            "cmd": 'printf spawned > "$STRESS_MEMORY_MARKER"',
                            "jobs_flag": "",
                            "hint": {
                                "est_duration_s": 1.0,
                                "rss_baseline_bytes": 1,
                                "classification": "light",
                                "preferred_inner_jobs": 1,
                            },
                        }
                    ],
                },
                handle,
            )
        stress_args = (
            "run", "--dag", stress_dag, "--stress", "2", "--max-cpus", "2",
            "--perf-dir", stress_store, "--unsafe-no-cgroups", "-q", NOPROF,
        )
        stress_outcomes = {
            engine: run(
                command,
                stress_args,
                {**extra, "STRESS_MEMORY_MARKER": stress_marker[engine]},
            )
            for engine, command in (("py", py), ("rs", rs))
        }
        if (
            all(outcome.returncode == 2 for outcome in stress_outcomes.values())
            and all("--stress 2: OK" in outcome.stderr for outcome in stress_outcomes.values())
            and all(
                "final planned expanded graph needs" in outcome.stderr
                and "exceeding the box memory budget" in outcome.stderr
                and "unbounded or overflowed" not in outcome.stderr
                for outcome in stress_outcomes.values()
            )
            and not any(os.path.exists(path) for path in stress_marker.values())
        ):
            rep.ok("memory:stress-profile-expanded-final-refusal")
        else:
            rep.bad(
                "memory:stress-profile-expanded-final-refusal",
                f"expected authored preflight success then finite planned-graph refusal; "
                f"outcomes={stress_outcomes} markers={stress_marker}",
            )


def compare_sweep_errors(py: list[str], rs: list[str], rep: Report) -> None:
    """``sweep --jobs`` malformed inputs must exit 2 with byte-identical stderr in both builds
    (guards the not-an-integer / malformed-range error-text parity)."""
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "dag.json")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write('{"steps": [{"group": "g", "job": "j", "cmd": "true"}]}')
        bad_jobs = ("abc", "3..1", "0..2", "1..x")
        for spec in bad_jobs:
            po = run(py, ("sweep", "--dag", path, "--step", "g.j", "--jobs", spec, UNBOXED))
            ro = run(rs, ("sweep", "--dag", path, "--step", "g.j", "--jobs", spec, UNBOXED))
            label = f"sweep-jobs-error:{spec}"
            if po.returncode != ro.returncode:
                rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}")
            elif po.returncode != 2:
                rep.bad(label, f"expected exit 2 for --jobs {spec!r}; got {po.returncode}")
            elif po.stderr != ro.stderr:
                rep.bad(
                    label,
                    f"stderr differs\n--- py ---\n{po.stderr}\n--- rs ---\n{ro.stderr}",
                )
            else:
                rep.ok(label)

        fixed = os.path.join(tmp, "self-managed.json")
        marker = {name: os.path.join(tmp, f"sweep-{name}-spawned") for name in ("py", "rs")}
        with open(fixed, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "steps": [
                        {
                            "group": "g",
                            "job": "fixed",
                            "cmd": 'printf spawned > "$SELF_MANAGED_MARKER"',
                            "jobs_flag": "",
                        }
                    ]
                },
                handle,
            )
        args = (
            "sweep", "--dag", fixed, "--step", "g.fixed", "--jobs", "1..2", NOPROF,
            UNBOXED,
        )
        po = run(py, args, {"SELF_MANAGED_MARKER": marker["py"]})
        ro = run(rs, args, {"SELF_MANAGED_MARKER": marker["rs"]})
        phrase = "offers no width channel"
        if (
            po.returncode == ro.returncode == 2
            and phrase in po.stderr
            and phrase in ro.stderr
            and not any(os.path.exists(path) for path in marker.values())
        ):
            rep.ok("sweep-jobs-error:self-managed-width")
        else:
            rep.bad(
                "sweep-jobs-error:self-managed-width",
                f"expected prompt refusal without spawn; py={po}\nrs={ro}\nmarkers={marker}",
            )


def compare_sweep_report_collisions(py: list[str], rs: list[str], rep: Report) -> None:
    """Report destinations must never destroy DAG/profile data, in either implementation."""

    with tempfile.TemporaryDirectory(prefix="sweep-report-collision-cross-") as tmp:
        dag = os.path.join(tmp, "dag.json")
        dag_source = (
            '{"steps":[{"group":"g","job":"j","cmd":"true",'
            '"jobs_flag":"--workers="}]}'
        )
        Path(dag).write_text(dag_source, encoding="utf-8")
        commands = {"py": py, "rs": rs}

        explicit: dict[str, Outcome] = {}
        explicit_stores = {
            name: os.path.join(tmp, f"explicit-{name}") for name in commands
        }
        for name, command in commands.items():
            explicit[name] = run(
                command,
                (
                    "sweep",
                    "--dag",
                    dag,
                    "--step",
                    "g.j",
                    "--jobs",
                    "1",
                    "--output-dir",
                    explicit_stores[name],
                    "--report-html",
                    dag,
                    "--unsafe-no-cgroups",
                ),
            )
        explicit_preserved = all(
            list(Path(store).glob("step_profiles_*.csv"))
            for store in explicit_stores.values()
        )
        if (
            all(outcome.returncode == 1 for outcome in explicit.values())
            and all(
                "report output resolves to DAG input" in outcome.stderr
                for outcome in explicit.values()
            )
            and Path(dag).read_text(encoding="utf-8") == dag_source
            and explicit_preserved
        ):
            rep.ok("sweep-report:explicit-collision-fails-after-persist")
        else:
            rep.bad(
                "sweep-report:explicit-collision-fails-after-persist",
                f"py={explicit['py']}\nrs={explicit['rs']}\n"
                f"dag={Path(dag).read_text(encoding='utf-8')!r} "
                f"profiles-preserved={explicit_preserved}",
            )

        defaults: dict[str, Outcome] = {}
        default_reports: dict[str, Path] = {}
        for name, command in commands.items():
            store = Path(tmp) / f"default-{name}"
            store.mkdir()
            report = store / "profile_report.html"
            report.write_bytes(b"do not overwrite")
            default_reports[name] = report
            defaults[name] = run(
                command,
                (
                    "sweep",
                    "--dag",
                    dag,
                    "--step",
                    "g.j",
                    "--jobs",
                    "1",
                    "--output-dir",
                    str(store),
                    "--unsafe-no-cgroups",
                ),
            )
        if (
            all(outcome.returncode == 0 for outcome in defaults.values())
            and all(
                "without dagrun report marker" in outcome.stderr
                for outcome in defaults.values()
            )
            and all(report.read_bytes() == b"do not overwrite" for report in default_reports.values())
        ):
            rep.ok("sweep-report:default-collision-is-warning")
        else:
            rep.bad(
                "sweep-report:default-collision-is-warning",
                f"py={defaults['py']}\nrs={defaults['rs']}\nreports={default_reports}",
            )


def _sweep_widths(text: str) -> set[int]:
    """Return widths from well-formed sweep rows, including humanized boxed RSS."""

    widths: set[int] = set()
    for line in text.splitlines():
        fields = line.split()
        # Unboxed rows render RSS as one ``-`` field; boxed rows render a value and unit (for
        # example ``760.0 KiB``).  Both are the same table schema, so do not mistake the extra
        # display token for a missing width sample.
        if len(fields) not in (6, 7) or not fields[0].isdigit():
            continue
        try:
            float(fields[1])
            float(fields[2])
            float(fields[3])
            if len(fields) == 6:
                if fields[4] != "-":
                    int(fields[4])
            else:
                float(fields[4])
                if fields[5] not in {"B", "KiB", "MiB", "GiB", "TiB"}:
                    continue
            float(fields[-1].removesuffix("x"))
        except ValueError:
            continue
        widths.add(int(fields[0]))
    return widths


def _has_sweep_header(text: str) -> bool:
    """Recognize the fixed sweep columns without coupling parity to display padding."""

    expected = ("jobs", "wall_s", "user_s", "sys_s", "rss_hwm", "speedup(vs", "j1)")
    return any(tuple(line.split()) == expected for line in text.splitlines())


def compare_sweep_success(py: list[str], rs: list[str], rep: Report) -> None:
    """Exercise a sweep and prove each table width is the width the guest actually received."""

    with tempfile.TemporaryDirectory(prefix="sweep-success-cross-") as tmp:
        observed = os.path.join(tmp, "observed-widths")
        guest = os.path.join(tmp, "record-width.sh")
        with open(guest, "w", encoding="utf-8") as handle:
            handle.write('#!/bin/sh\nprintf "%s\\n" "$1" >> "$OBSERVED_WIDTHS"\n')
        os.chmod(guest, 0o700)
        path = os.path.join(tmp, "dag.json")
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "steps": [
                        {
                            "group": "g",
                            "job": "j",
                            "cmd": shlex.quote(guest),
                            "jobs_flag": "--workers=",
                            "env": {"OBSERVED_WIDTHS": observed},
                        }
                    ]
                },
                handle,
            )
        args = (
            "sweep", "--dag", path, "--step", "g.j", "--jobs", "1..2", NOPROF, UNBOXED,
        )
        po = run(py, args)
        try:
            with open(observed, encoding="utf-8") as handle:
                py_observed = handle.read().splitlines()
        except OSError:
            py_observed = []
        try:
            os.unlink(observed)
        except FileNotFoundError:
            pass
        ro = run(rs, args)
        try:
            with open(observed, encoding="utf-8") as handle:
                rs_observed = handle.read().splitlines()
        except OSError:
            rs_observed = []
        if (
            po.returncode == ro.returncode == 0
            and _has_sweep_header(po.stdout)
            and _has_sweep_header(ro.stdout)
            and _sweep_widths(po.stdout) == _sweep_widths(ro.stdout) == {1, 2}
            and py_observed == rs_observed == ["--workers=1", "--workers=2"]
        ):
            rep.ok("sweep:successful-table-and-guest-width")
        else:
            rep.bad(
                "sweep:successful-table-and-guest-width",
                f"py={po.returncode}:{po.stdout!r}:{po.stderr!r}\n"
                f"rs={ro.returncode}:{ro.stdout!r}:{ro.stderr!r}\n"
                f"observed py={py_observed!r} rs={rs_observed!r}",
            )


def compare_spawn_failure(py: list[str], rs: list[str], rep: Report) -> None:
    """Invalid spawn input must fail promptly and eager-cancel an in-flight sibling."""

    with tempfile.TemporaryDirectory(prefix="spawn-failure-cross-") as tmp:
        path = os.path.join(tmp, "dag.json")
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "steps": [
                        {
                            "group": "g",
                            "job": "slow",
                            "cmd": "sleep 10",
                            "hint": {"est_duration_s": 100.0},
                        },
                        {
                            "group": "g",
                            "job": "gate",
                            "cmd": "sleep 0.3",
                            "hint": {"est_duration_s": 50.0},
                        },
                        {
                            "group": "g",
                            "job": "spawn",
                            "cmd": "true",
                            "deps": ["g.gate"],
                            "env": {"BAD": "embedded\0nul"},
                        }
                    ]
                },
                handle,
            )
        args = (
            "run",
            "--dag",
            path,
            "-q",
            "-s2",
            "-j2",
            NOPROF,
            NOFB,
            "--unsafe-no-cgroups",
        )
        po = run(py, args, timeout_s=8.0)
        ro = run(rs, args, timeout_s=8.0)
        py_text = po.stdout + po.stderr
        rs_text = ro.stdout + ro.stderr
        if (
            po.returncode == ro.returncode == 1
            and "spawn failed" in py_text
            and "spawn failed" in rs_text
            and "ABORT" in py_text
            and "ABORT" in rs_text
            and "Traceback" not in py_text
            and "Exception in thread" not in py_text
        ):
            rep.ok("run:spawn-failure-returns")
        else:
            rep.bad(
                "run:spawn-failure-returns",
                f"py={po.returncode}:{po.stdout!r}:{po.stderr!r}\n"
                f"rs={ro.returncode}:{ro.stdout!r}:{ro.stderr!r}",
            )


def _summary_sync_case(
    py: list[str],
    rs: list[str],
    rep: Report,
    *,
    label: str,
    machine: str,
    container: str,
    csv: str,
    dag: dict[str, object],
) -> None:
    """One profile-SUMMARY parity case against a fixed synthetic store + DAG. Asserts, across the
    Python and Rust builds:

    * ``summary build`` (store CSV -> summary JSON) is BYTE-IDENTICAL — the serialization correctness
      core (float-as-fixed-string, canonical bucket + reservoir order, FNV-1a subsample order).
    * ``summary plan`` FROM that summary equals ``plan --perf-dir`` from the raw store in EACH build
      (the summary recomputes the same estimates as the direct reader) AND is byte-identical across
      builds.
    * ``summary merge`` is byte-identical AND COMMUTATIVE (merge(a,b) == merge(b,a)), across builds —
      the mergeable-summary property that makes concurrent contributions order-independent."""
    extra = {
        "DAGRUN_MACHINE_ID": machine,
        "DAGRUN_CONTAINER_CLASS": container,
    }
    with tempfile.TemporaryDirectory() as tmp:
        store = os.path.join(tmp, "store")
        os.makedirs(store, exist_ok=True)
        with open(os.path.join(store, f"step_profiles_{machine}_{container}.csv"), "w") as fh:
            fh.write(csv)
        dag_path = os.path.join(tmp, "dag.json")
        with open(dag_path, "w") as fh:
            fh.write(json.dumps(dag))

        # 1) summary build: byte-identical.
        build = ("summary", "build", "--perf-dir", store)
        pb, rb = run(py, build, extra), run(rs, build, extra)
        if pb.returncode != 0 or rb.returncode != 0:
            rep.bad(f"summary-build:{label}", f"exit py={pb.returncode} rs={rb.returncode} "
                                               f"(py {pb.stderr!r}; rs {rb.stderr!r})")
            return
        if pb.stdout != rb.stdout:
            rep.bad(f"summary-build:{label}",
                    f"summary JSON not byte-identical\n--- py ---\n{pb.stdout}\n--- rs ---\n{rb.stdout}")
            return
        rep.ok(f"summary-build:{label}")

        fallback_extra = dict(extra)
        fallback_extra["DAGRUN_PROFILE_DIR"] = store
        empty_assignments = ("summary", "build", "--perf-dir=", "--out=")
        empty_py = run(py, empty_assignments, fallback_extra)
        empty_rs = run(rs, empty_assignments, fallback_extra)
        if (
            empty_py.returncode == empty_rs.returncode == 0
            and empty_py.stdout == empty_rs.stdout == pb.stdout
            and not empty_py.stderr
            and not empty_rs.stderr
        ):
            rep.ok(f"summary-build-empty-assignment:{label}")
        else:
            rep.bad(
                f"summary-build-empty-assignment:{label}",
                f"py={empty_py.returncode}:{empty_py.stdout!r}:{empty_py.stderr!r}\n"
                f"rs={empty_rs.returncode}:{empty_rs.stdout!r}:{empty_rs.stderr!r}",
            )

        s_path = os.path.join(tmp, "summary.json")
        with open(s_path, "w") as fh:
            fh.write(pb.stdout)

        # A single input is a valid identity merge in both editions. Keep this aligned with the
        # `FILE [FILE ...]` action schema and its "one or more" help text.
        single_args = ("summary", "merge", "--", s_path)
        single_py, single_rs = run(py, single_args), run(rs, single_args)
        if (
            single_py.returncode == single_rs.returncode == 0
            and single_py.stdout == single_rs.stdout == pb.stdout
        ):
            rep.ok(f"summary-merge-single-delimited:{label}")
        else:
            rep.bad(
                f"summary-merge-single-delimited:{label}",
                f"py={single_py.returncode}:{single_py.stdout!r}:{single_py.stderr!r}\n"
                f"rs={single_rs.returncode}:{single_rs.stdout!r}:{single_rs.stderr!r}",
            )

        # 2) summary plan (from the summary) == plan --perf-dir (from the store), per build + cross.
        for fmt in ("text", "json"):
            sp = ("summary", "plan", "--dag", dag_path, "--summary", s_path, "--format", fmt)
            pp = ("plan", "--dag", dag_path, "--perf-dir", store, "--format", fmt)
            psp, rsp = run(py, sp, extra), run(rs, sp, extra)
            ppp = run(py, pp, extra)
            if psp.stdout != ppp.stdout:
                rep.bad(f"summary-plan-eq-store:{label}/{fmt}",
                        f"summary plan != store plan (py)\n--- summary ---\n{psp.stdout}\n"
                        f"--- store ---\n{ppp.stdout}")
            else:
                rep.ok(f"summary-plan-eq-store:{label}/{fmt}")
            if psp.stdout != rsp.stdout:
                rep.bad(f"summary-plan:{label}/{fmt}",
                        f"summary plan not byte-identical\n--- py ---\n{psp.stdout}\n--- rs ---\n{rsp.stdout}")
            else:
                rep.ok(f"summary-plan:{label}/{fmt}")

        # 3) summary merge: byte-identical + commutative, across builds. Split the store into two
        # halves so the merge actually combines distinct contributions.
        lines = csv.splitlines(keepends=True)
        header, body = lines[0], lines[1:]
        mid = max(1, len(body) // 2)
        a_csv, b_csv = header + "".join(body[:mid]), header + "".join(body[mid:])
        a_path, b_path = os.path.join(tmp, "a.json"), os.path.join(tmp, "b.json")
        for half, path in ((a_csv, a_path), (b_csv, b_path)):
            half_store = os.path.join(tmp, "half")
            os.makedirs(half_store, exist_ok=True)
            with open(os.path.join(half_store, f"step_profiles_{machine}_{container}.csv"), "w") as fh:
                fh.write(half)
            out = run(py, ("summary", "build", "--perf-dir", half_store), extra)
            with open(path, "w") as fh:
                fh.write(out.stdout)
        for order, name in (((a_path, b_path), "ab"), ((b_path, a_path), "ba")):
            args = ("summary", "merge", *order)
            pm, rm = run(py, args), run(rs, args)
            if pm.stdout != rm.stdout:
                rep.bad(f"summary-merge:{label}/{name}",
                        f"merge not byte-identical\n--- py ---\n{pm.stdout}\n--- rs ---\n{rm.stdout}")
            else:
                rep.ok(f"summary-merge:{label}/{name}")
        # Commutativity: merge(a,b) == merge(b,a) in the python build (already cross-checked identical).
        m_ab = run(py, ("summary", "merge", a_path, b_path)).stdout
        m_ba = run(py, ("summary", "merge", b_path, a_path)).stdout
        if m_ab == m_ba:
            rep.ok(f"summary-merge-commutative:{label}")
        else:
            rep.bad(f"summary-merge-commutative:{label}",
                    f"merge(a,b) != merge(b,a)\n--- ab ---\n{m_ab}\n--- ba ---\n{m_ba}")


#: A fixed synthetic store for :func:`compare_memory_feedback`, carrying the applied-cap and
#: event-counter provenance the censoring-aware reader needs. All three steps recorded the SAME
#: 8 GiB applied cap and never OOMed:
#:
#: * ``g.learned`` sat at 2 GiB with every counter at zero, so its peaks are real observations.
#: * ``g.pinned`` peaked EXACTLY at the ceiling every time: identical ``memory_max_bytes``,
#:   opposite meaning. A build that ignored the provenance would derive a number for it too.
#: * ``g.throttled`` peaked at the same comfortable 2 GiB as ``g.learned`` but recorded
#:   ``memory_events_high > 0`` — a SOFT ceiling was throttling it into direct reclaim, so its
#:   peak is suppressed. Byte-for-byte its ``peak_bytes`` and ``memory_max_bytes`` match
#:   ``g.learned``; only the counter differs, so a build that reads ``max``/``oom_kill`` but not
#:   ``high`` gives it the same learned cap and this fixture catches that.
#: * ``g.failed`` peaked at the same comfortable 2 GiB with every counter at zero, but every one
#:   of its six runs was KILLED: ``ok=false``, ``returncode=137`` (SIGKILL, which is what an
#:   ancestor-scope OOM kill looks like from inside the reader). Its peaks are where the step got
#:   to before it died.
#: * ``g.four`` recorded only FOUR comfortable samples. Nothing here passes a sample threshold to
#:   either build, so this step is the only thing that observes the SHIPPED
#:   ``DEFAULT_MIN_UNCENSORED_SAMPLES``, and the two builds disagreeing about it shows up as a
#:   difference in its line.
_CENSORED_STORE_CSV = (
    "step,peak_bytes,memory_max_bytes,memory_events_high,memory_events_max,"
    "memory_events_oom,memory_events_oom_kill,ok,returncode\n"
    + "".join(
        f"g.learned,{2 * 1024**3},{8 * 1024**3},0,0,0,0,true,0\n" for _ in range(6)
    )
    + "".join(
        f"g.pinned,{8 * 1024**3},{8 * 1024**3},0,0,0,0,true,0\n" for _ in range(6)
    )
    + "".join(
        f"g.throttled,{2 * 1024**3},{8 * 1024**3},4,0,0,0,true,0\n" for _ in range(6)
    )
    + "".join(
        f"g.failed,{2 * 1024**3},{8 * 1024**3},0,0,0,0,false,137\n" for _ in range(6)
    )
    + "".join(
        f"g.four,{2 * 1024**3},{8 * 1024**3},0,0,0,0,true,0\n" for _ in range(4)
    )
)

#: What ``g.learned``'s six identical 2147483648 B samples must produce, written out literally:
#: the 9/10 nearest-rank percentile of six equal values is that value, plus the 20% margin
#: (429496729 B). Naming it here rather than recomputing it from the builds' own constants is
#: what makes this a contract instead of a restatement of whatever they currently do.
_LEARNED_EXPECTED_BYTES = 2576980377

#: Every step that this store makes the builds DECLINE authors a hint of 17179869184 B, above the
#: largest peak any of them recorded (8589934592 B for ``g.pinned``, 2147483648 B for the rest).
#: That is deliberate: a decline keeps the LARGER of the authored hint and the floor the evidence
#: proves, so authoring above the floor is what makes "keeping the authored hint" the true verdict
#: for these four and leaves the floor half of the rule to
#: :func:`_memory_feedback_floor_survives_a_decline`, where it is the only thing under test.
_CENSORED_DAG = {
    "steps": [
        {"group": "g", "job": "learned", "cmd": "true"},
        {"group": "g", "job": "pinned", "cmd": "true",
         "hint": {"rss_baseline_bytes": 17179869184}},
        {"group": "g", "job": "throttled", "cmd": "true",
         "hint": {"rss_baseline_bytes": 17179869184}},
        {"group": "g", "job": "failed", "cmd": "true",
         "hint": {"rss_baseline_bytes": 17179869184}},
        {"group": "g", "job": "four", "cmd": "true",
         "hint": {"rss_baseline_bytes": 17179869184}},
    ],
}


def _memory_feedback_reaches_admission(
    py: list[str], rs: list[str], rep: Report, tmp: str, extra: dict[str, str]
) -> None:
    """Prove the derived baseline changes what ``--max-mem`` admission models, identically.

    ONE step, so the modeled worst case is that step's own cap and does not depend on this
    host's CPU count (only the step-count ceiling in the same line does, which is why the
    footprint substring is matched rather than the whole line). Its six uncensored samples
    peaked at 21474836480 B under a 107374182400 B cap, and the DAG authors a larger
    42949672960 B hint.

    Without the flag the authored floor survives and admission models 53687091200 B. With the
    explicit flag, six uncensored samples are enough to lower that floor: the censoring-aware
    estimate adds the 20% feedback margin (25769803776 B), then admission models 32212254720 B.
    Both are named literally: this proves that ordinary feedback cannot lower an author's number
    while the deliberately requested, evidence-gated path still can.
    """
    store = os.path.join(tmp, "admission-store")
    os.makedirs(store, exist_ok=True)
    csv_name = f"step_profiles_{SYNTH_MACHINE}_{SYNTH_CONTAINER}.csv"
    with open(os.path.join(store, csv_name), "w", encoding="utf-8") as fh:
        fh.write(
            "step,peak_bytes,memory_max_bytes,memory_events_high,memory_events_max,"
            "memory_events_oom,memory_events_oom_kill\n"
            + "".join("g.learned,21474836480,107374182400,0,0,0,0\n" for _ in range(6))
        )
    dag_path = os.path.join(tmp, "admission-dag.json")
    with open(dag_path, "w", encoding="utf-8") as fh:
        fh.write(json.dumps({"steps": [{
            "group": "g", "job": "learned", "cmd": "true",
            "hint": {"rss_baseline_bytes": 42949672960},
        }]}))
    args = (
        "run", "--dag", dag_path, "--perf-dir", store, "--no-profile",
        "--unsafe-no-cgroups", "-q", "--max-mem", "400G",
    )

    def footprint(outcome: Outcome) -> str | None:
        for line in outcome.stderr.splitlines():
            match = re.search(r"worst-case (\d+) bytes", line)
            if match:
                return match.group(1)
        return None

    off = {"py": run(py, args, extra), "rs": run(rs, args, extra)}
    on_args = (*args, "--profile-memory-feedback")
    on = {"py": run(py, on_args, extra), "rs": run(rs, on_args, extra)}
    seen = {
        f"{engine}/{state}": footprint(outcome)
        for state, group in (("off", off), ("on", on))
        for engine, outcome in group.items()
    }
    if (
        seen != {"py/off": "53687091200", "rs/off": "53687091200",
                 "py/on": "32212254720", "rs/on": "32212254720"}
        or any(o.returncode != 0 for o in (*off.values(), *on.values()))
        or "rss_baseline_bytes=25769803776" not in on["py"].stderr
        or "rss_baseline_bytes=25769803776" not in on["rs"].stderr
    ):
        rep.bad("memory-feedback/reaches-max-mem-admission", f"footprints={seen!r}")
    else:
        rep.ok("memory-feedback/reaches-max-mem-admission")


def _memory_feedback_decline_keeps_authored_floor_at_admission(
    py: list[str], rs: list[str], rep: Report, tmp: str, extra: dict[str, str]
) -> None:
    """Prove a censored DECLINE preserves the authored floor in both feedback modes.

    ONE step has six runs, every one pinned to its 8589934592 B cap: the all-censored history the
    opt-in path exists to reject. The DAG authors 42949672960 B.

    Ordinary feedback must not lower that authored floor, so without the flag admission models
    53687091200 B. With the flag the step is explicitly declined and the same authored figure
    still reaches admission. Both are named literally: neither the ordinary path nor a declined
    opt-in path may derive a cap from a capped observation.

    The authored 42949672960 B is deliberately ABOVE the 8589934592 B floor those peaks prove, so
    the decline really does land on the author's number here. The opposite arrangement is
    :func:`_memory_feedback_floor_survives_a_decline`.
    """
    store = os.path.join(tmp, "decline-store")
    os.makedirs(store, exist_ok=True)
    csv_name = f"step_profiles_{SYNTH_MACHINE}_{SYNTH_CONTAINER}.csv"
    with open(os.path.join(store, csv_name), "w", encoding="utf-8") as fh:
        fh.write(
            "step,peak_bytes,memory_max_bytes,memory_events_high,memory_events_max,"
            "memory_events_oom,memory_events_oom_kill\n"
            + "".join("g.pinned,8589934592,8589934592,0,0,0,0\n" for _ in range(6))
        )
    dag_path = os.path.join(tmp, "decline-dag.json")
    with open(dag_path, "w", encoding="utf-8") as fh:
        fh.write(json.dumps({"steps": [{
            "group": "g", "job": "pinned", "cmd": "true",
            "hint": {"rss_baseline_bytes": 42949672960},
        }]}))
    args = (
        "run", "--dag", dag_path, "--perf-dir", store, "--no-profile",
        "--unsafe-no-cgroups", "-q", "--max-mem", "400G",
    )

    def footprint(outcome: Outcome) -> str | None:
        for line in outcome.stderr.splitlines():
            match = re.search(r"worst-case (\d+) bytes", line)
            if match:
                return match.group(1)
        return None

    off = {"py": run(py, args, extra), "rs": run(rs, args, extra)}
    on_args = (*args, "--profile-memory-feedback")
    on = {"py": run(py, on_args, extra), "rs": run(rs, on_args, extra)}
    seen = {
        f"{engine}/{state}": footprint(outcome)
        for state, group in (("off", off), ("on", on))
        for engine, outcome in group.items()
    }
    if (
        seen != {"py/off": "53687091200", "rs/off": "53687091200",
                 "py/on": "53687091200", "rs/on": "53687091200"}
        or any(o.returncode != 0 for o in (*off.values(), *on.values()))
        or "g.pinned: keeping the authored hint" not in on["py"].stderr
        or "g.pinned: keeping the authored hint" not in on["rs"].stderr
    ):
        rep.bad("memory-feedback/a-decline-keeps-the-authored-floor", f"footprints={seen!r}")
    else:
        rep.ok("memory-feedback/a-decline-keeps-the-authored-floor")


def _memory_feedback_floor_survives_a_decline(
    py: list[str], rs: list[str], rep: Report, tmp: str, extra: dict[str, str]
) -> None:
    """Prove a decline keeps the FLOOR its censored peaks prove, identically in both builds.

    The other half of the decline contract, and the half that is easy to over-correct into. ONE
    step whose ten recorded runs were every one of them pinned to the ceiling they were given:
    nine at 8589934592 B and one at 34359738368 B. The DAG authors 1073741824 B — BELOW what the
    store proves the step has already used, which is the direction
    :func:`_memory_feedback_decline_keeps_authored_floor_at_admission` does not cover.

    Without the flag the censoring-blind feedback fits the 9/10 nearest-rank percentile of those
    peaks — the ninth smallest, 8589934592 B — and admission models 10737418240 B. With the flag
    the step is DECLINED, because no peak says what it wanted; but one of them says it reached
    34359738368 B, so that is the floor the decline keeps and admission models 42949672960 B.

    So turning the flag on RAISES the modelled footprint here, which is the only direction a
    censored sample may move it. Both numbers are named literally: a build that restored the
    authored figure alone would report a far smaller second number, and a build that added the
    20% margin to the floor — a floor is a fact about the past, not an estimate of the next run —
    would report a different one again.
    """
    store = os.path.join(tmp, "floor-store")
    os.makedirs(store, exist_ok=True)
    csv_name = f"step_profiles_{SYNTH_MACHINE}_{SYNTH_CONTAINER}.csv"
    with open(os.path.join(store, csv_name), "w", encoding="utf-8") as fh:
        fh.write(
            "step,peak_bytes,memory_max_bytes,memory_events_high,memory_events_max,"
            "memory_events_oom,memory_events_oom_kill\n"
            + "".join("g.pinned,8589934592,8589934592,0,0,0,0\n" for _ in range(9))
            + "g.pinned,34359738368,34359738368,0,0,0,0\n"
        )
    dag_path = os.path.join(tmp, "floor-dag.json")
    with open(dag_path, "w", encoding="utf-8") as fh:
        fh.write(json.dumps({"steps": [{
            "group": "g", "job": "pinned", "cmd": "true",
            "hint": {"rss_baseline_bytes": 1073741824},
        }]}))
    args = (
        "run", "--dag", dag_path, "--perf-dir", store, "--no-profile",
        "--unsafe-no-cgroups", "-q", "--max-mem", "400G",
    )

    def footprint(outcome: Outcome) -> str | None:
        for line in outcome.stderr.splitlines():
            match = re.search(r"worst-case (\d+) bytes", line)
            if match:
                return match.group(1)
        return None

    off = {"py": run(py, args, extra), "rs": run(rs, args, extra)}
    on_args = (*args, "--profile-memory-feedback")
    on = {"py": run(py, on_args, extra), "rs": run(rs, on_args, extra)}
    seen = {
        f"{engine}/{state}": footprint(outcome)
        for state, group in (("off", off), ("on", on))
        for engine, outcome in group.items()
    }
    said = "g.pinned: no estimate; rss_baseline_bytes=34359738368, the proven floor"
    if (
        seen != {"py/off": "10737418240", "rs/off": "10737418240",
                 "py/on": "42949672960", "rs/on": "42949672960"}
        or any(o.returncode != 0 for o in (*off.values(), *on.values()))
        or said not in on["py"].stderr
        or said not in on["rs"].stderr
    ):
        rep.bad(
            "memory-feedback/a-decline-keeps-the-proven-floor",
            f"footprints={seen!r}\n--- py ---\n{on['py'].stderr}\n--- rs ---\n{on['rs'].stderr}",
        )
    else:
        rep.ok("memory-feedback/a-decline-keeps-the-proven-floor")


def compare_memory_feedback(py: list[str], rs: list[str], rep: Report) -> None:
    """Prove the OPT-IN censoring-aware memory feedback agrees across builds, and is really off.

    Against a fixed synthetic store where three steps recorded peaks under the same applied cap
    — one comfortably, one pinned to the ceiling, one throttled at a soft ceiling:

    * without the flag neither build says anything about memory feedback (it is opt-in, and a
      default-on reader would be a silent behaviour change for every existing store);
    * with the flag both builds emit BYTE-IDENTICAL decision lines, so the classification, the
      percentile, the margin and the wording are one contract rather than two;
    * the pinned step keeps its authored hint — which it authored ABOVE the floor its peaks
      prove — and the reason names censoring, while the comfortable step gets a LITERALLY NAMED
      number, proving the two are not treated alike;
    * the throttled step, whose ``peak_bytes`` and ``memory_max_bytes`` are byte-identical to the
      comfortable one and which differs only in ``memory_events_high``, also keeps its hint;
    * asking for the flag while ``--no-profile-feedback`` disables the reader says so in both
      builds instead of doing nothing quietly.
    """
    extra = {
        "DAGRUN_MACHINE_ID": SYNTH_MACHINE,
        "DAGRUN_CONTAINER_CLASS": SYNTH_CONTAINER,
    }
    with tempfile.TemporaryDirectory() as tmp:
        store = os.path.join(tmp, "store")
        os.makedirs(store, exist_ok=True)
        csv_name = f"step_profiles_{SYNTH_MACHINE}_{SYNTH_CONTAINER}.csv"
        with open(os.path.join(store, csv_name), "w", encoding="utf-8") as fh:
            fh.write(_CENSORED_STORE_CSV)
        dag_path = os.path.join(tmp, "dag.json")
        with open(dag_path, "w", encoding="utf-8") as fh:
            fh.write(json.dumps(_CENSORED_DAG))
        base = (
            "run", "--dag", dag_path, "--perf-dir", store, "--no-profile",
            "--unsafe-no-cgroups", "-q",
        )

        def lines(outcome: Outcome) -> list[str]:
            return [
                line for line in outcome.stderr.splitlines()
                if "--profile-memory-feedback:" in line
            ]

        off = {"py": run(py, base, extra), "rs": run(rs, base, extra)}
        for engine, outcome in off.items():
            if outcome.returncode != 0 or lines(outcome):
                rep.bad(
                    f"memory-feedback:{engine}/off-by-default",
                    f"exit={outcome.returncode}; unexpected report={lines(outcome)}",
                )
            else:
                rep.ok(f"memory-feedback:{engine}/off-by-default")

        on = (*base, "--profile-memory-feedback")
        po, ro = run(py, on, extra), run(rs, on, extra)
        py_lines, rs_lines = lines(po), lines(ro)
        if po.returncode != 0 or ro.returncode != 0:
            rep.bad(
                "memory-feedback/exit",
                f"exit py={po.returncode} rs={ro.returncode}\n{po.stderr}\n{ro.stderr}",
            )
        elif py_lines != rs_lines:
            rep.bad(
                "memory-feedback/identical",
                "decision lines differ\n--- py ---\n"
                + "\n".join(py_lines)
                + "\n--- rs ---\n"
                + "\n".join(rs_lines),
            )
        else:
            rep.ok("memory-feedback/identical")
        learned = [line for line in py_lines if "g.learned:" in line]
        pinned = [line for line in py_lines if "g.pinned:" in line]
        throttled = [line for line in py_lines if "g.throttled:" in line]
        if (
            len(learned) != 1
            or len(pinned) != 1
            or f"rss_baseline_bytes={_LEARNED_EXPECTED_BYTES}" not in learned[0]
            or "keeping the authored hint" not in pinned[0]
            or "censored by its applied cap" not in pinned[0]
        ):
            rep.bad(
                "memory-feedback/censoring-separates-equal-peaks",
                "learned=" + repr(learned) + " pinned=" + repr(pinned),
            )
        else:
            rep.ok("memory-feedback/censoring-separates-equal-peaks")
        # The soft ceiling. Same peak, same cap, same absent OOM as g.learned; only
        # memory_events_high differs, so this fails in exactly the build that skips that counter.
        if (
            len(throttled) != 1
            or "keeping the authored hint" not in throttled[0]
            or "rss_baseline_bytes=" in throttled[0]
        ):
            rep.bad(
                "memory-feedback/soft-ceiling-throttling-censors",
                "throttled=" + repr(throttled),
            )
        else:
            rep.ok("memory-feedback/soft-ceiling-throttling-censors")

        # A step whose six runs were KILLED. Byte-for-byte its peak, cap and counters match
        # g.learned; only ok/returncode differ, so this fails in exactly the build that reads a
        # peak measured by a dead run as an observation of demand.
        failed = [line for line in py_lines if "g.failed:" in line]
        if (
            len(failed) != 1
            or "keeping the authored hint" not in failed[0]
            or "rss_baseline_bytes=" in failed[0]
            or "6 sample(s) recorded a step that FAILED" not in failed[0]
        ):
            rep.bad("memory-feedback/a-failed-run-is-not-evidence", "failed=" + repr(failed))
        else:
            rep.ok("memory-feedback/a-failed-run-is-not-evidence")

        # FOUR comfortable samples, and no threshold passed to either build: the only place the
        # SHIPPED minimum-sample count is observable end to end. The numbers are named literally,
        # so a build that shipped anything but 5 produces a different line here.
        four = [line for line in py_lines if "g.four:" in line]
        if (
            len(four) != 1
            or "keeping the authored hint" not in four[0]
            or "only 4 uncensored sample(s); 5 required" not in four[0]
        ):
            rep.bad("memory-feedback/five-uncensored-samples-are-required", "four=" + repr(four))
        else:
            rep.ok("memory-feedback/five-uncensored-samples-are-required")

        _memory_feedback_reaches_admission(py, rs, rep, tmp, extra)
        _memory_feedback_decline_keeps_authored_floor_at_admission(py, rs, rep, tmp, extra)
        _memory_feedback_floor_survives_a_decline(py, rs, rep, tmp, extra)

        inert = (*base, "--profile-memory-feedback", "--no-profile-feedback")
        pi, ri = run(py, inert, extra), run(rs, inert, extra)
        py_inert, rs_inert = lines(pi), lines(ri)
        if (
            pi.returncode != 0
            or ri.returncode != 0
            or py_inert != rs_inert
            or len(py_inert) != 1
            or "no estimate is derived" not in py_inert[0]
        ):
            rep.bad(
                "memory-feedback/inert-under-no-profile-feedback",
                f"exit py={pi.returncode} rs={ri.returncode}\n"
                f"py={py_inert!r}\nrs={rs_inert!r}",
            )
        else:
            rep.ok("memory-feedback/inert-under-no-profile-feedback")


def compare_summary_sync(py: list[str], rs: list[str], rep: Report) -> None:
    """Prove the mergeable profile SUMMARY (the profile-artifact sync feature's correctness core) is
    byte-identical py<->rs for serialization, merge, and recomputed plan — on BOTH the feedback store
    (contention discount) and the multi-width speedup store (speedup curves)."""
    _summary_sync_case(
        py, rs, rep, label="feedback", machine=SYNTH_MACHINE, container=SYNTH_CONTAINER,
        csv=_FEEDBACK_STORE_CSV, dag=_FEEDBACK_DAG,
    )
    _summary_sync_case(
        py, rs, rep, label="speedup", machine=_SPEEDUP_MACHINE, container=_SPEEDUP_CONTAINER,
        csv=_speedup_store_csv(), dag=_SPEEDUP_DAG,
    )


def compare_mixed_local_summary_publishers(
    py: list[str], rs: list[str], rep: Report
) -> None:
    """Prove the local summary backend's lock preserves simultaneous mixed-engine uploads."""

    dag = (
        '{"steps":['
        '{"group":"g","job":"a","cmd":"true"},'
        '{"group":"g","job":"b","cmd":"true"},'
        '{"group":"g","job":"c","cmd":"true"}'
        "]}"
    )
    with tempfile.TemporaryDirectory(prefix="dagrun-mixed-summary-") as tmp:
        dag_path = os.path.join(tmp, "dag.json")
        Path(dag_path).write_text(dag, encoding="utf-8")
        shared = os.path.join(tmp, "shared")
        args = (
            "run",
            "--dag",
            dag_path,
            "--no-profile",
            NOFB,
            "--profile-sync",
            f"local:{shared}",
            "--profile-sync-direction",
            "upload",
            "--unsafe-no-cgroups",
            "--max-steps",
            "1",
            "-q",
        )
        commands = (py, rs, py, rs)
        with ThreadPoolExecutor(max_workers=len(commands)) as executor:
            outcomes = list(executor.map(lambda command: run(command, args), commands))
        if any(outcome.returncode != 0 for outcome in outcomes):
            rep.bad(
                "summary-sync:mixed-concurrent-publishers",
                "\n".join(f"run {index}: {outcome}" for index, outcome in enumerate(outcomes)),
            )
            return
        summaries = sorted(Path(shared).glob("summary_*.json"))
        if len(summaries) != 1:
            rep.bad(
                "summary-sync:mixed-concurrent-publishers",
                f"expected one summary, found {summaries!r}",
            )
            return
        path = str(summaries[0])
        stats = {
            "py": run(py, ("summary", "stats", path)),
            "rs": run(rs, ("summary", "stats", path)),
        }
        document = json.loads(summaries[0].read_text(encoding="utf-8"))
        sample_count = sum(
            len(bucket.get("samples", []))
            for bucket in document.get("buckets", [])
            if isinstance(bucket, dict) and isinstance(bucket.get("samples"), list)
        )
        if (
            sample_count != 12
            or any(outcome.returncode != 0 for outcome in stats.values())
            or stats["py"].stdout != stats["rs"].stdout
        ):
            rep.bad(
                "summary-sync:mixed-concurrent-publishers",
                f"samples={sample_count}/12\npy={stats['py']}\nrs={stats['rs']}",
            )
        else:
            rep.ok("summary-sync:mixed-concurrent-publishers")


INVOCATIONS: tuple[Invocation, ...] = (
    Invocation("version", ("--version",)),
    Invocation("help", ("--help",)),
    Invocation("noargs", ()),
    # Each installed edition carries a standalone generated guide. Content isolation is checked
    # separately; language-specific install/API fragments intentionally differ.
    Invocation("userguide", ("--userguide",)),
    # `capabilities` prints each engine's machine-readable enforcement manifest (which guards it
    # actually implements: cpu_affinity, cpu_bandwidth, cpu_timeout, memory_max, oom_detection,
    # pids_guard, run_timeout, wall_timeout, and write_domains) — per LANE, since most of them are
    # a cgroup read or write and simply do not happen on an uncontained run. The
    # WHOLE POINT is that the two engines must enforce the SAME set, so the manifests are asserted
    # byte-identical. This is the recurrence guard for the historical gap where the Rust runner
    # silently did NOT enforce `cpu_timeout` while the Python runner did: with this check, that
    # divergence is a `cross` failure in ANY environment (it needs no cgroup boxing, so it fires on
    # ubuntu-latest CI where the behavioral boxed tests can only loud-skip). The boxed smoke tests
    # in each build (boxing_smoke / cpu_timeout_smoke, and the Python pytest equivalents) anchor
    # these declarations to real behavior wherever a cgroup-v2 + systemd --user scope exists.
    Invocation("capabilities", ("capabilities",)),
)

#: Invocations whose stdout must be BYTE-IDENTICAL across builds (not just exit-code equal).
_BYTE_IDENTICAL_INVOCATIONS = frozenset({"version", "capabilities"})


def compare_invocations(py: list[str], rs: list[str], rep: Report) -> None:
    for inv in INVOCATIONS:
        po = run(py, inv.args)
        ro = run(rs, inv.args)
        if po.returncode != ro.returncode:
            rep.bad(f"invocation/{inv.name}", f"exit py={po.returncode} rs={ro.returncode}")
        elif inv.name in _BYTE_IDENTICAL_INVOCATIONS and po.stdout != ro.stdout:
            rep.bad(
                f"invocation/{inv.name}",
                f"stdout not byte-identical\n--- py ---\n{po.stdout}\n--- rs ---\n{ro.stdout}",
            )
        else:
            rep.ok(f"invocation/{inv.name}")


def compare_cli_schema(py: list[str], rs: list[str], rep: Report) -> None:
    """Require both CLIs to expose the complete supported command/flag inventory.

    Exit-code-only help checks historically allowed one implementation to omit whole features.
    This semantic schema check is deliberately explicit: a one-sided command or flag is a parity
    failure even when human-facing wrapping/wording differs.
    """
    command_flags: dict[str, tuple[str, ...]] = {
        "run": (
            "--dag", "--max-steps", "--max-cpus", "--cores", "--cpuset", "--pin",
            "--max-mem",
            "--selected", "--ignore-selected-deps",
            "--args", "--stress", "--perf-dir", "--no-profile", "--profile", "--planner",
            "--profile-timeseries",
            "--show-plan", "--no-profile-feedback", "--profile-memory-feedback",
            "--profile-sync",
            "--profile-sync-direction", "--keep-going", "--run-timeout",
            "--admission",
            "--allow-cgroup-failure",
            "--unsafe-no-cgroups", "--allow-unwise-nest-dagruns", "--small-default-cap", "--quiet",
        ),
        "box": (
            "--mem", "--timeout", "--cores", "--label", "--perf-dir",
            "--allow-cgroup-failure", "--quiet",
        ),
        "sweep": (
            "--dag", "--step", "--jobs", "--target-time", "--repeat", "--output-dir",
            "--perf-dir", "--no-profile", "--report-html", "--no-report",
            "--profile-timeseries", "--perf-record", "--perf-window", "--wprof-window",
            "--profiler-sudo",
            "--allow-cgroup-failure", "--unsafe-no-cgroups",
        ),
        "plan": ("--dag", "--planner", "--max-mem", "--format", "--perf-dir", "--no-profile-feedback"),
        "pin-run": ("--cores", "--tag"),
    }
    command_forbidden_flags: dict[str, tuple[str, ...]] = {
        # `--jobs` is a hidden 0.13 compatibility alias; `--only` is retired rather than aliased.
        "run": ("--jobs", "--only"),
        # `box` is a front door for ONE command, not a second `run`. A `--dag` here would mean
        # the two editions had grown different ideas of what the subcommand is for.
        "box": ("--dag", "--selected", "--ignore-selected-deps", "--stress"),
    }
    summary_action_contracts: dict[str, tuple[tuple[str, ...], tuple[str, ...]]] = {
        "build": (
            ("--perf-dir", "--out", "--reservoir-cap"),
            ("--summary", "--dag", "--planner", "--max-mem", "--format"),
        ),
        "merge": (
            ("FILE", "--out", "--reservoir-cap"),
            ("--perf-dir", "--summary", "--dag", "--planner", "--max-mem", "--format"),
        ),
        "plan": (
            ("--summary", "--dag", "--planner", "--max-mem", "--format"),
            ("--perf-dir", "--out", "--reservoir-cap"),
        ),
        "stats": (
            ("FILE",),
            ("--perf-dir", "--out", "--reservoir-cap", "--summary", "--dag", "--planner"),
        ),
    }
    summary_top_phrases = (
        "build a plan from a summary json and dag",
        "merge one or more summary json files",
    )
    top_commands = (
        "run", "box", "sweep", "plan", "list", "ascii", "dot", "json", "yaml", "summary",
        "pin-run", "quickstart", "capabilities",
    )
    for engine, command in (("py", py), ("rs", rs)):
        top = run(command, ("--help",))
        missing_commands = [name for name in top_commands if name not in top.stdout]
        if top.returncode != 0 or missing_commands:
            rep.bad(
                f"cli-schema:{engine}/commands",
                f"exit={top.returncode}; missing commands={missing_commands}\n{top.stdout}",
            )
        else:
            rep.ok(f"cli-schema:{engine}/commands")
        summary_top = run(command, ("summary", "--help"))
        normalized_summary_top = " ".join(summary_top.stdout.lower().split())
        missing_summary_phrases = [
            phrase for phrase in summary_top_phrases if phrase not in normalized_summary_top
        ]
        stale_backend_claim = "plan a summary sync from a backend spec" in normalized_summary_top
        if summary_top.returncode != 0 or missing_summary_phrases or stale_backend_claim:
            rep.bad(
                f"cli-schema:{engine}/summary-top",
                f"exit={summary_top.returncode}; missing={missing_summary_phrases}; "
                f"stale backend claim={stale_backend_claim}\n{summary_top.stdout}",
            )
        else:
            rep.ok(f"cli-schema:{engine}/summary-top")
        sweep_help = run(command, ("sweep", "--help"))
        normalized_sweep_help = " ".join(sweep_help.stdout.lower().split())
        sweep_contract = (
            "single-step dense mode",
            "target-time graph mode",
            "one at a time in stable topological order",
            "pass 1 always completes",
            "every started pass finishes",
            "powers of two through the physical-core count",
            "physical-core and logical-thread counts",
            "integer midpoints",
            "scaling_model_*.json",
            "profile_report.html",
            "traces/<run_id>.csv",
            "backward-compatible alias for --output-dir",
            "does not redirect terminal or child-process output",
        )
        missing_sweep_phrases = [
            phrase for phrase in sweep_contract if phrase not in normalized_sweep_help
        ]
        if sweep_help.returncode != 0 or missing_sweep_phrases:
            rep.bad(
                f"cli-schema:{engine}/sweep-explanation",
                f"exit={sweep_help.returncode}; missing phrases={missing_sweep_phrases}\n"
                f"{sweep_help.stdout}",
            )
        else:
            rep.ok(f"cli-schema:{engine}/sweep-explanation")
        for subcommand, flags in command_flags.items():
            outcome = run(command, (subcommand, "--help"))
            missing = [flag for flag in flags if flag not in outcome.stdout]
            unexpected = [
                flag for flag in command_forbidden_flags.get(subcommand, ())
                if flag in outcome.stdout
            ]
            if outcome.returncode != 0 or missing or unexpected:
                rep.bad(
                    f"cli-schema:{engine}/{subcommand}",
                    f"exit={outcome.returncode}; missing flags={missing}; "
                    f"unexpected flags={unexpected}\n{outcome.stdout}",
                )
            else:
                rep.ok(f"cli-schema:{engine}/{subcommand}")
        for action, (required, forbidden) in summary_action_contracts.items():
            outcome = run(command, ("summary", action, "--help"))
            missing = [flag for flag in required if flag not in outcome.stdout]
            unexpected = [flag for flag in forbidden if flag in outcome.stdout]
            action_usage = f"summary {action}"
            generic = "summary <action>" in outcome.stdout
            if (
                outcome.returncode != 0
                or missing
                or unexpected
                or action_usage not in outcome.stdout
                or generic
            ):
                rep.bad(
                    f"cli-schema:{engine}/summary-{action}",
                    f"exit={outcome.returncode}; missing flags={missing}; "
                    f"unexpected flags={unexpected}; "
                    f"action usage present={action_usage in outcome.stdout}; "
                    f"generic={generic}\n{outcome.stdout}",
                )
            else:
                rep.ok(f"cli-schema:{engine}/summary-{action}")
    summary_invalid_invocations: tuple[tuple[str, tuple[str, ...]], ...] = (
        ("build-unexpected-positional", ("summary", "build", "unexpected")),
        (
            "plan-unexpected-positional",
            (
                "summary",
                "plan",
                "unexpected",
                "--summary",
                "missing-summary.json",
                "--dag",
                "missing-dag.json",
            ),
        ),
        ("stats-extra-file", ("summary", "stats", "one.json", "two.json")),
        ("merge-missing-file", ("summary", "merge")),
        ("build-cap-malformed", ("summary", "build", "--reservoir-cap", "nope")),
        ("build-cap-underscore", ("summary", "build", "--reservoir-cap", "1_0")),
        ("build-cap-leading-space", ("summary", "build", "--reservoir-cap", " 10")),
        ("build-cap-trailing-space", ("summary", "build", "--reservoir-cap", "10 ")),
        ("build-cap-unicode-digits", ("summary", "build", "--reservoir-cap", "١٠")),
        ("build-cap-zero", ("summary", "build", "--reservoir-cap", "0")),
        (
            "build-cap-out-of-i64-range",
            ("summary", "build", "--reservoir-cap", "9223372036854775808"),
        ),
        (
            "merge-cap-negative",
            ("summary", "merge", "missing.json", "--reservoir-cap", "-1"),
        ),
        (
            "plan-bad-planner",
            (
                "summary",
                "plan",
                "--summary",
                "missing-summary.json",
                "--dag",
                "missing-dag.json",
                "--planner",
                "unknown",
            ),
        ),
        (
            "plan-bad-format",
            (
                "summary",
                "plan",
                "--summary",
                "missing-summary.json",
                "--dag",
                "missing-dag.json",
                "--format",
                "xml",
            ),
        ),
        ("build-missing-value", ("summary", "build", "--perf-dir")),
        (
            "build-delimited-positional",
            ("summary", "build", "--", "--looks-like-an-option"),
        ),
        (
            "plan-missing-value-before-flag",
            ("summary", "plan", "--summary", "--dag", "missing-dag.json"),
        ),
    )
    for label, args in summary_invalid_invocations:
        po, ro = run(py, args), run(rs, args)
        if (
            po.returncode == ro.returncode == 2
            and not po.stdout
            and not ro.stdout
            and po.stderr
            and ro.stderr
        ):
            rep.ok(f"cli-schema:summary-invalid/{label}")
        else:
            rep.bad(f"cli-schema:summary-invalid/{label}", f"py={po}\nrs={ro}")
    po = run(py, ("run", "--da", "missing.json"))
    ro = run(rs, ("run", "--da", "missing.json"))
    if po.returncode == ro.returncode == 2:
        rep.ok("cli-schema:abbreviation-refused")
    else:
        rep.bad("cli-schema:abbreviation-refused", f"py={po.returncode}; rs={ro.returncode}")


def compare_args_stress(py: list[str], rs: list[str], rep: Report) -> None:
    """Cross-check the previously Python-only passthrough and stress surfaces."""
    with tempfile.TemporaryDirectory(prefix="dagrun-cross-args-stress-") as td:
        passthrough_path = os.path.join(td, "passthrough.json")
        with open(passthrough_path, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "steps": [
                        {
                            "group": "test",
                            "job": "unit",
                            "cmd": 'test "{args}" = "--case xyz"',
                            "hint": {"hard_mem_max_bytes": 1},
                        }
                    ]
                },
                handle,
            )
        passthrough = (
            "run", "--dag", passthrough_path, "--args=--case xyz", "--unsafe-no-cgroups",
            "--no-profile", "-q",
        )
        po, ro = run(py, passthrough), run(rs, passthrough)
        if po.returncode == ro.returncode == 0:
            rep.ok("args:substitution")
        else:
            rep.bad(
                "args:substitution",
                f"expected both 0; py={po.returncode} rs={ro.returncode}\npy:{po.stderr}\nrs:{ro.stderr}",
            )

        invalid_path = os.path.join(td, "no-placeholder.json")
        with open(invalid_path, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "steps": [
                        {
                            "group": "test",
                            "job": "unit",
                            "cmd": "true",
                            "hint": {"hard_mem_max_bytes": 1},
                        }
                    ]
                },
                handle,
            )
        invalid = (
            "run", "--dag", invalid_path, "--args=x", "--unsafe-no-cgroups", "--no-profile", "-q",
        )
        po, ro = run(py, invalid), run(rs, invalid)
        if po.returncode == ro.returncode == 2 and "no selected step declares" in po.stderr and "no selected step declares" in ro.stderr:
            rep.ok("args:undeclared-refused")
        else:
            rep.bad("args:undeclared-refused", f"py={po}\nrs={ro}")

        stress = (
            "run", "--dag", invalid_path, "--stress", "3", "--unsafe-no-cgroups",
            "--no-profile", "-q",
        )
        po, ro = run(py, stress), run(rs, stress)
        # Parallel completion lines and the observed peak overlap are intentionally
        # nondeterministic for three near-instant `true` commands. Compare the stable pass ratio
        # and configured ceilings while only requiring each observed peak to be in range. Exact
        # overlap is exercised by the barrier-backed scheduler cases elsewhere in this harness.
        def stress_report(stdout: str) -> tuple[str, str, int] | None:
            lines = stdout.splitlines()
            start = next(
                (index for index, line in enumerate(lines) if line.startswith("stress results (")),
                len(lines),
            )
            report = lines[start:]
            if len(report) != 3:
                return None
            match = re.fullmatch(
                r"  maximum concurrent steps: ([0-9]+) (\(--max-steps .+\))",
                report[2],
            )
            if match is None:
                return None
            return report[0] + "\n" + report[1], match.group(2), int(match.group(1))

        py_report, rs_report = stress_report(po.stdout), stress_report(ro.stdout)
        if (
            po.returncode == ro.returncode == 0
            and py_report is not None
            and rs_report is not None
            and py_report[:2] == rs_report[:2]
            and "3/3 passed" in py_report[0]
            and 1 <= py_report[2] <= 3
            and 1 <= rs_report[2] <= 3
        ):
            rep.ok("stress:three-pass-ratio")
        else:
            rep.bad(
                "stress:three-pass-ratio",
                f"py={po.returncode}:{py_report!r}\nrs={ro.returncode}:{rs_report!r}",
            )

        invalid_stress = (
            "run", "--dag", invalid_path, "--stress", "0", "--unsafe-no-cgroups", "--no-profile",
        )
        po, ro = run(py, invalid_stress), run(rs, invalid_stress)
        if po.returncode == ro.returncode == 2 and "must be >= 1" in po.stderr and "must be >= 1" in ro.stderr:
            rep.ok("stress:nonpositive-refused")
        else:
            rep.bad("stress:nonpositive-refused", f"py={po}\nrs={ro}")

        oversized_stress = (
            "run",
            "--dag",
            invalid_path,
            "--stress",
            "1000000",
            "--unsafe-no-cgroups",
            "--no-profile",
        )
        po, ro = run(py, oversized_stress), run(rs, oversized_stress)
        expansion_phrase = (
            "expansion would create 1000000 generated DAG nodes/control units, "
            "exceeding safety limit 100000"
        )
        if (
            po.returncode == ro.returncode == 2
            and expansion_phrase in po.stderr
            and expansion_phrase in ro.stderr
        ):
            rep.ok("stress:generated-node-cap-refuses-oversized-fanout")
        else:
            rep.bad("stress:generated-node-cap-refuses-oversized-fanout", f"py={po}\nrs={ro}")

        hard_cores = (
            "run",
            "--dag",
            invalid_path,
            "--cores",
            "1",
            "--unsafe-no-cgroups",
            "--no-profile",
            "-q",
        )
        ledger = {"DAGRUN_CORE_LEDGER": os.path.join(td, "hard-refusal-ledger.json")}
        po, ro = run(py, hard_cores, ledger), run(rs, hard_cores, ledger)
        if (
            po.returncode == ro.returncode == 3
            and "hard cgroup cpuset unavailable; refusing to run" in po.stderr
            and "hard cgroup cpuset unavailable; refusing to run" in ro.stderr
        ):
            rep.ok("cores:unboxed-soft-affinity-refused")
        else:
            rep.bad("cores:unboxed-soft-affinity-refused", f"py={po}\nrs={ro}")


def _cpu_guest_dag(
    widths: Sequence[int],
    *,
    duration_s: float,
    hardcoded_workers: int | None = None,
    cgroup_parent_levels: int | None = None,
    barrier_participants: int | None = None,
) -> dict[str, object]:
    """One shared DAG whose output path is supplied per engine through the environment."""

    steps: list[object] = []
    for index, width in enumerate(widths):
        tag = f"footprint.s{index}"
        command = (
            f"{shlex.quote(sys.executable)} {shlex.quote(CPU_FOOTPRINT_GUEST)} "
            f"--output \"$CPU_FOOTPRINT_OUTPUT\" --step {shlex.quote(tag)} "
            f"--duration-s {duration_s:g} --sample-ms 10 --start-delay-ms 500"
        )
        jobs_flag: str | None = "--workers="
        if hardcoded_workers is not None:
            command += f" --workers={hardcoded_workers}"
            jobs_flag = ""
        if cgroup_parent_levels is not None:
            command += (
                f" --cgroup-parent-levels {cgroup_parent_levels} --cgroup-sample-ms 25"
            )
        if barrier_participants is not None:
            command += (
                " --barrier-file \"$CPU_FOOTPRINT_BARRIER\" "
                f"--barrier-participants {barrier_participants} --barrier-timeout-s 10"
            )
        steps.append(
            {
                "group": "footprint",
                "job": f"s{index}",
                "desc": f"CPU footprint width {width}",
                "cmd": command,
                "jobs_flag": jobs_flag,
                "timeout": 15,
                "cpu_timeout": 120,
                "hint": {
                    "preferred_inner_jobs": width,
                    "hard_mem_max_bytes": 536_870_912,
                    "est_duration_s": duration_s,
                },
            }
        )
    return {"steps": steps}


def _cpu_facts(log_path: str) -> tuple[FootprintStats, CpuFootprintFacts]:
    events = load_cpu_events([Path(log_path)])
    stats = analyze_cpu_footprint(events, bucket_ns=25_000_000)
    workers: dict[str, set[tuple[int, int]]] = {}
    for record in events:
        if record.get("event") != "worker_start":
            continue
        step, worker, pid = record.get("step"), record.get("worker"), record.get("pid")
        if not isinstance(step, str):
            continue
        if not isinstance(worker, int) or isinstance(worker, bool):
            continue
        if not isinstance(pid, int) or isinstance(pid, bool):
            continue
        workers.setdefault(step, set()).add((worker, pid))
    facts = CpuFootprintFacts(
        completed_steps=len(stats.step_intervals),
        workers_per_step=tuple(sorted(len(entries) for entries in workers.values())),
        max_live_steps=stats.max_live_steps,
        max_live_workers=stats.max_live_workers,
    )
    return stats, facts


def compare_run_parallel_limits(py: list[str], rs: list[str], rep: Report) -> None:
    """Cross-check active-step overlap, per-step width caps, and outer CPU bandwidth."""

    with tempfile.TemporaryDirectory(prefix="dagrun-cross-cpu-limits-") as td:
        invalid_dag = os.path.join(td, "valid.json")
        Path(invalid_dag).write_text(
            '{"steps":[{"group":"g","job":"ok","cmd":"true"}]}', encoding="utf-8"
        )
        invalid_cases: tuple[tuple[str, tuple[str, ...]], ...] = (
            ("max-steps-spaced-zero", ("-s", "0")),
            ("max-steps-bare-zero", ("-s0",)),
            ("max-steps-invalid", ("--max-steps", "nope")),
            ("max-steps-i64-overflow", ("--max-steps", "9223372036854775808")),
            ("max-cpus-spaced-zero", ("-j", "0")),
            ("max-cpus-bare-zero", ("-j0",)),
            ("max-cpus-invalid", ("--max-cpus", "nope")),
            ("max-cpus-i64-overflow", ("--max-cpus", "9223372036854775808")),
        )
        for label, flags in invalid_cases:
            args = (
                "run", "--dag", invalid_dag, *flags, NOPROF, NOFB, "--unsafe-no-cgroups",
            )
            po, ro = run(py, args), run(rs, args)
            if po.returncode == ro.returncode == 2 and po.stderr and ro.stderr:
                rep.ok(f"run-limits:invalid/{label}")
            else:
                rep.bad(f"run-limits:invalid/{label}", f"py={po}\nrs={ro}")

        conflict_args = (
            "run", "--dag", invalid_dag, "--max-cpus", "2", "--jobs", "3",
            NOPROF, NOFB, "--unsafe-no-cgroups",
        )
        po, ro = run(py, conflict_args), run(rs, conflict_args)
        conflict_message = "--max-cpus and legacy --jobs disagree"
        if (
            po.returncode == ro.returncode == 2
            and conflict_message in po.stderr
            and conflict_message in ro.stderr
        ):
            rep.ok("run-limits:invalid/max-cpus-legacy-jobs-conflict")
        else:
            rep.bad(
                "run-limits:invalid/max-cpus-legacy-jobs-conflict",
                f"expected exit 2 with {conflict_message!r}; py={po}\nrs={ro}",
            )

        self_managed_dag = os.path.join(td, "self-managed.json")
        marker = {name: os.path.join(td, f"run-{name}-spawned") for name in ("py", "rs")}
        Path(self_managed_dag).write_text(
            json.dumps(
                {
                    "steps": [
                        {
                            "group": "g",
                            "job": "fixed",
                            "cmd": 'printf spawned > "$SELF_MANAGED_MARKER"',
                            "jobs_flag": "",
                            "hint": {"preferred_inner_jobs": 5},
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )
        args = (
            "run", "--dag", self_managed_dag, "--max-cpus", "2", NOPROF, NOFB,
            "--unsafe-no-cgroups", "-q",
        )
        po = run(py, args, {"SELF_MANAGED_MARKER": marker["py"]})
        ro = run(rs, args, {"SELF_MANAGED_MARKER": marker["rs"]})
        phrase = "cannot lower guest parallelism"
        if (
            po.returncode == ro.returncode == 2
            and phrase in po.stderr
            and phrase in ro.stderr
            and not any(os.path.exists(path) for path in marker.values())
        ):
            rep.ok("run-limits:self-managed-over-budget-refused")
        else:
            rep.bad(
                "run-limits:self-managed-over-budget-refused",
                f"expected prompt refusal without spawn; py={po}\nrs={ro}\nmarkers={marker}",
            )

        whitespace_dag = os.path.join(td, "whitespace-jobs-flag.json")
        Path(whitespace_dag).write_text(
            json.dumps(
                {
                    "steps": [
                        {
                            "group": "g",
                            "job": "fixed",
                            "cmd": "sh -c 'test \"$#\" -eq 0' _",
                            "jobs_flag": "   ",
                            "hint": {"preferred_inner_jobs": 2},
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )
        args = (
            "run", "--dag", whitespace_dag, "--max-cpus", "2", NOPROF, NOFB,
            "--unsafe-no-cgroups", "-q",
        )
        po, ro = run(py, args), run(rs, args)
        if po.returncode == ro.returncode == 0:
            rep.ok("run-limits:whitespace-jobs-flag-does-not-append-width")
        else:
            rep.bad(
                "run-limits:whitespace-jobs-flag-does-not-append-width",
                f"whitespace-only jobs_flag appended an argument; py={po}\nrs={ro}",
            )

        cases: tuple[
            tuple[str, tuple[int, ...], tuple[str, ...], CpuFootprintFacts], ...
        ] = (
            (
                "step-cap-two-cpu-budget-four",
                (2, 2, 2, 2),
                ("-s", "2", "-j", "4"),
                CpuFootprintFacts(4, (2, 2, 2, 2), 2, 4),
            ),
            (
                "bare-step-four-cpu-budget-two",
                (1, 1, 1, 1),
                ("-s4", "-j2"),
                CpuFootprintFacts(4, (1, 1, 1, 1), 4, 4),
            ),
            (
                "two-width-eight-steps-share-eight-core-budget",
                (8, 8),
                ("-s2", "-j8"),
                CpuFootprintFacts(2, (8, 8), 2, 16),
            ),
            (
                "authored-width-capped-to-j",
                (5,),
                ("--max-steps=4", "--max-cpus=2"),
                CpuFootprintFacts(1, (2,), 1, 2),
            ),
            (
                "legacy-hidden-jobs-alias",
                (1, 1, 1),
                ("--max-steps=3", "--jobs=2"),
                CpuFootprintFacts(3, (1, 1, 1), 3, 3),
            ),
            (
                "canonical-and-legacy-equal",
                (1, 1, 1),
                ("--max-steps=3", "--max-cpus=2", "--jobs=2"),
                CpuFootprintFacts(3, (1, 1, 1), 3, 3),
            ),
            # #36 dag-core-budget-split. These watch the ONE coupling the split deliberately
            # kept: with `-s` absent, the active-step ceiling defaults to the RESOLVED `-j`.
            #
            # Every case in the list above passes an explicit `-s`, so none of THEM can see it,
            # but that is a statement about this list and not about the tree. Two cases earlier
            # in this function do omit `-s` without observing the ceiling (one refuses at exit
            # 2, one only checks the exit status), and `memory:nonbinding-max-mem-keeps-cpu-base`
            # over in compare_memory_hardening genuinely does observe it: `--max-cpus 1` with no
            # `-s`, asserting a base ceiling of 1. Raise the default and that case goes red too
            # (verified: defaulting both engines to the host CPU count reddens it alongside
            # `...-cpu-budget-one` and `...-two-hundred`). So the tree was never blind to the
            # default; what it could not see is the default following a `-j` ABOVE one -- the
            # direction a user actually feels -- and it saw none of it against live guest
            # evidence. A user who types `-j1` for bandwidth also gets a serial DAG, and a user
            # who types `-j3` also gets three-way overlap. Both numbers are literal here, and
            # the two directions are separate cases so a regression that pinned the default at 1
            # (or at the CPU count) cannot pass by satisfying the other one.
            (
                "absent-step-ceiling-follows-cpu-budget-one",
                (1, 1, 1),
                ("-j", "1"),
                CpuFootprintFacts(3, (1, 1, 1), 1, 1),
            ),
            (
                "absent-step-ceiling-follows-cpu-budget-three",
                (1, 1, 1),
                ("-j", "3"),
                CpuFootprintFacts(3, (1, 1, 1), 3, 3),
            ),
            # The hidden 0.13 alias must reach the same default. A 0.13 script that kept its CPU
            # budget while silently losing its step ceiling is precisely the "old spelling still
            # works but means something else" outcome the split was supposed to avoid.
            (
                "absent-step-ceiling-follows-legacy-jobs-three",
                (1, 1, 1),
                ("--jobs=3",),
                CpuFootprintFacts(3, (1, 1, 1), 3, 3),
            ),
        )
        for label, widths, flags, expected in cases:
            dag_path = os.path.join(td, f"{label}.json")
            barrier_participants = expected.max_live_steps if expected.max_live_steps > 1 else None
            Path(dag_path).write_text(
                json.dumps(
                    _cpu_guest_dag(
                        widths,
                        duration_s=1.5 if expected.max_live_steps > 1 else 0.4,
                        barrier_participants=barrier_participants,
                    )
                ),
                encoding="utf-8",
            )
            logs = {name: os.path.join(td, f"{label}-{name}.jsonl") for name in ("py", "rs")}
            args = (
                "run", "--dag", dag_path, *flags, "-q", NOPROF, NOFB,
                "--unsafe-no-cgroups",
            )
            extra = {
                name: {
                    "CPU_FOOTPRINT_OUTPUT": path,
                    "DAGRUN_NO_STEP_LOGS": "1",
                    "CPU_FOOTPRINT_BARRIER": os.path.join(td, f"{label}-{name}.barrier"),
                }
                for name, path in logs.items()
            }
            outcomes = {"py": run(py, args, extra["py"]), "rs": run(rs, args, extra["rs"])}
            if any(outcome.returncode != 0 for outcome in outcomes.values()):
                rep.bad(f"run-limits:{label}", f"py={outcomes['py']}\nrs={outcomes['rs']}")
                continue
            try:
                analyzed = {name: _cpu_facts(path) for name, path in logs.items()}
            except (OSError, ValueError) as exc:
                rep.bad(f"run-limits:{label}", f"could not analyze guest evidence: {exc}")
                continue
            facts = {name: pair[1] for name, pair in analyzed.items()}
            verdicts = {
                name: check_cpu_limits(
                    pair[0],
                    max_steps=expected.max_live_steps,
                    max_workers=expected.max_live_workers,
                )
                for name, pair in analyzed.items()
            }
            if facts["py"] != facts["rs"]:
                rep.bad(
                    f"run-limits:{label}",
                    f"normalized facts py={facts['py']} rs={facts['rs']}",
                )
            elif facts["py"] != expected:
                rep.bad(
                    f"run-limits:{label}",
                    f"expected {expected}, observed {facts['py']} "
                    "(CPU-ID union intentionally ignored)",
                )
            elif not all(verdict.ok for verdict in verdicts.values()):
                rep.bad(f"run-limits:{label}", f"limit verdicts={verdicts}")
            else:
                rep.ok(f"run-limits:{label}")

        # The magnitude the shipped README and user guide actually promise: `run -j200` alone
        # permits TWO HUNDRED active nodes. Launching two hundred spinning guests to watch that
        # happen would be a host-size lottery, so it is pinned where both engines ANNOUNCE the
        # number instead: with `--max-mem` present each prints the base active-step ceiling it
        # derived, and from `-j200` with no `-s` that must read 200 in both. Every guest-evidence
        # case above uses 1 or 3, so a clamp on the default -- `min(max_cpus, 8)` is the
        # one-token version -- leaves all of them green while falsifying the shipped sentence.
        # Only the base is asserted: the modelled memory ceiling is a property of the host, and
        # _sizing_details already refuses any line whose final ceiling is not min(base, memory).
        gib = 1024**3
        wide_default = {
            "mem_cap_factor": 1.0,
            "mem_cap_floor_bytes": 0,
            "outer_mem_safety_factor": 1.0,
            "steps": [
                {
                    "group": "wide-default",
                    "job": f"s{index}",
                    "cmd": "true",
                    "jobs_flag": "",
                    "hint": {"hard_mem_max_bytes": gib},
                }
                for index in range(2)
            ],
        }
        wide_path = os.path.join(td, "absent-step-ceiling-two-hundred.json")
        Path(wide_path).write_text(json.dumps(wide_default), encoding="utf-8")
        wide_args = (
            "run", "--dag", wide_path, "--max-mem", "16G", "-j", "200",
            "--unsafe-no-cgroups", "-q", NOPROF, NOFB,
        )
        pwide, rwide = run(py, wide_args), run(rs, wide_args)
        pwd, rwd = _sizing_details(pwide.stderr), _sizing_details(rwide.stderr)
        if (
            pwide.returncode == rwide.returncode == 0
            and pwd is not None
            and pwd == rwd
            and pwd[3] == 200
        ):
            rep.ok("run-limits:absent-step-ceiling-follows-cpu-budget-two-hundred")
        else:
            rep.bad(
                "run-limits:absent-step-ceiling-follows-cpu-budget-two-hundred",
                f"expected both engines to derive base active-step ceiling 200 from -j200 with "
                f"no -s; py={pwide} details={pwd} rs={rwide} details={rwd}",
            )


def _boxing_capability_unavailable(outcome: Outcome) -> bool:
    combined = outcome.stdout + outcome.stderr
    return outcome.returncode == 3 and (
        "systemd --user scope is unavailable" in combined
        or "scope setup was attempted and failed" in combined
        or ("cgroup boxing could not be established" in combined and "unavailable" in combined)
    )


#: What a skipped boxed leg says when the parent scheduler is itself unboxed.
_NO_DELEGABLE_SUBTREE = "the outer scheduler has no cgroup subtree to delegate"

#: Both engines print this on stderr exactly when they have established per-step cgroups, in
#: their own two-level scope or in a parent-owned delegated root. Nothing else prints it.
_BOXING_ACTIVE_BANNER = "cgroup boxing ACTIVE"


def _skip_without_delegable_subtree(check: str, rep: Report, what: str) -> bool:
    """Record a boxed leg as a boxing skip, without running it, when no box can exist.

    Under an unboxed outer scheduler a nested engine honours ``DAGRUN_DELEGATED_UNBOXED=1`` and
    runs the step uncontained whatever the fixture asks for, so the leg would compare two unboxed
    runs. Counting that as agreement would claim boxed coverage that was never exercised.
    """

    if not _parent_offers_only_unboxed_delegation():
        return False
    print(f"cross[dagrun]: SKIP {what} ({check}): {_NO_DELEGABLE_SUBTREE}")
    rep.skip(check, "boxing", _NO_DELEGABLE_SUBTREE)
    return True


def _engines_that_did_not_box(outcomes: Mapping[str, Outcome]) -> list[str]:
    """Name the engines whose boxed run never announced an established cgroup box.

    A boxed leg whose engines agree without the banner has compared unboxed runs. That must be a
    failure rather than coverage, whatever the reason the box was missing.
    """

    return sorted(
        name for name, out in outcomes.items() if _BOXING_ACTIVE_BANNER not in out.stderr
    )


_BUILD_WIDTH_RE = re.compile(r"build width: (.+)$", re.MULTILINE)
#: The width the STEP received, echoed by the step's own command.
_STEP_WIDTH_RE = re.compile(r"WIDTH=(\d+)")


def compare_operator_build_width(py: list[str], rs: list[str], rep: Report) -> None:
    """Both builds must resolve the operator-vs-containment build width the same way, and say so.

    ``derive_build_jobs`` used to overwrite ``CARGO_BUILD_JOBS`` unconditionally, so an operator
    who set it — having sized a memory cap against exactly that pool — lost it with no word said.
    Reading the variable back as intent is not the fix either: the runner SETS it — both engines
    pass ``--setenv=CARGO_BUILD_JOBS`` into the scope, which is the path this check drives — so the
    in-scope process would read its own derivation as an instruction and stop refining per step.

    This is the one mechanism that can see both engines resolve it, on the boxing path where the
    substitution actually happens. It checks two observables per leg, because the sentence alone
    is cheap to keep true while the wiring rots:

    * the SENTENCE each build prints, which must be identical and name the right authority; and
    * the width the STEP actually received, echoed by the step's own command. Unstated, that must
      be strictly below the scope-wide number (downward refinement still happening); stated, it
      must be exactly the operator's number, unrefined.

    Deleting the ``DAGRUN_OPERATOR_BUILD_JOBS`` forwarding in one engine leaves the sentence
    correct and breaks the second observable, which is the whole reason it is here.
    """
    if _parent_offers_only_unboxed_delegation():
        for leg in ("stated", "unstated"):
            print(
                f"cross[dagrun]: SKIP boxed build-width differential ({leg}): "
                "the outer scheduler has no cgroup subtree to delegate"
            )
            rep.skip(
                f"operator-build-width:{leg}",
                "boxing",
                "the outer scheduler has no cgroup subtree to delegate",
            )
        return

    with tempfile.TemporaryDirectory(prefix="dagrun-cross-build-width-") as td:
        dag_path = os.path.join(td, "dag.json")
        # A hard 2 GiB per-step cap makes the derived per-step width small and host-independent
        # relative to the scope's, so "refined downward" is observable rather than incidental.
        Path(dag_path).write_text(
            '{"steps": [{"group": "g", "job": "a", '
            '"cmd": "echo WIDTH=$CARGO_BUILD_JOBS", "timeout": 60, '
            '"hint": {"hard_mem_max_bytes": 2147483648}}]}',
            encoding="utf-8",
        )
        args = ("run", "--dag", dag_path, "-s1", "-j16", NOPROF, NOFB)
        base = {
            "DAGRUN_NO_STEP_LOGS": "1",
            "DAGRUN_FORCE_SCOPE_ATTEMPT": "1",
        }
        for leg, extra_env, want in (
            ("stated", {"CARGO_BUILD_JOBS": "200"}, "honouring CARGO_BUILD_JOBS=200"),
            ("unstated", {}, "no CARGO_BUILD_JOBS in the environment"),
        ):
            env = {**base, **extra_env}
            outcomes = {
                "py": run(py, args, env, inherit_runner_authority=True),
                "rs": run(rs, args, env, inherit_runner_authority=True),
            }
            label = f"operator-build-width:{leg}"
            if all(_boxing_capability_unavailable(out) for out in outcomes.values()):
                print(
                    f"cross[dagrun]: SKIP boxed build-width differential ({leg}): "
                    "cgroup-v2 + a working systemd --user scope are unavailable"
                )
                rep.skip(
                    label,
                    "boxing",
                    "cgroup-v2 + a working systemd --user scope are unavailable",
                )
                continue
            unboxed = _engines_that_did_not_box(outcomes)
            if unboxed:
                rep.bad(
                    label,
                    f"a boxed leg must run boxed; no {_BOXING_ACTIVE_BANNER!r} from {unboxed}: "
                    f"outcomes={outcomes}",
                )
                continue
            combined = {name: out.stdout + out.stderr for name, out in outcomes.items()}
            said = {name: _BUILD_WIDTH_RE.findall(text) for name, text in combined.items()}
            applied = {name: _STEP_WIDTH_RE.findall(text) for name, text in combined.items()}
            delegated = _inside_delegated_harness()
            if not delegated and (not said["py"] or not said["rs"]):
                rep.bad(
                    label,
                    "each build must announce which width governs; "
                    f"py={said['py']!r} rs={said['rs']!r}",
                )
                continue
            if said["py"] != said["rs"]:
                rep.bad(label, f"differing announcement py={said['py']!r} rs={said['rs']!r}")
                continue
            if not delegated and want not in said["py"][0]:
                rep.bad(label, f"expected {want!r} in {said['py'][0]!r}")
                continue
            if not applied["py"] or not applied["rs"]:
                rep.bad(
                    label,
                    "the step must report the width it actually received; "
                    f"py={applied['py']!r} rs={applied['rs']!r}",
                )
                continue
            if applied["py"] != applied["rs"]:
                rep.bad(label, f"differing step width py={applied['py']!r} rs={applied['rs']!r}")
                continue
            step_width = int(applied["py"][0])
            if leg == "stated":
                if step_width != 200:
                    rep.bad(
                        label,
                        "an operator's stated width must reach the step unrefined; "
                        f"got {step_width}",
                    )
                    continue
            elif step_width >= 16:
                rep.bad(
                    label,
                    "with nothing stated the step must be refined DOWNWARD from the scope's "
                    f"16-core width by its own 2 GiB cap; got {step_width}. This is what a "
                    "lost DAGRUN_OPERATOR_BUILD_JOBS forwarding looks like.",
                )
                continue
            rep.ok(label)


def compare_jobs_env_width(py: list[str], rs: list[str], rep: Report) -> None:
    """Prove the allocated width reaches the child through an environment channel.

    The narrow leg catches planner/cap bugs (preferred four under a one-core admission must become
    one), the roomy leg catches implementations that always collapse to one, and the boxed leg
    catches the late scope-wide ``CARGO_BUILD_JOBS`` export overwriting the step allocation.
    Refusal cases prove missing or malformed channels fail before spawning the child.

    The two boxed legs count only when both engines announce an established cgroup box. Under an
    unboxed outer scheduler no box can exist, so they are recorded as boxing skips and not run.
    Inside a parent-owned delegated root the systemd ``--setenv`` export never runs, but the
    fixture's own ``CARGO_BUILD_JOBS=8`` still reaches the step unless the jobs-env export wins, so
    the boxed narrow leg keeps its meaning there.
    """

    with tempfile.TemporaryDirectory(prefix="dagrun-cross-jobs-env-") as td:
        root = Path(td)
        dag_path = root / "dag.json"
        dag_path.write_text(
            json.dumps(
                {
                    "steps": [
                        {
                            "group": "g",
                            "job": "a",
                            "cmd": 'printf "%s" "$CARGO_BUILD_JOBS" > "$OBSERVED_PATH"',
                            "jobs_flag": "",
                            "timeout": 60,
                            "hint": {"preferred_inner_jobs": 4},
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )

        def width_leg(label: str, max_cpus: int, expected: str, *, boxed: bool) -> None:
            check = f"jobs-env:{label}"
            if boxed and _skip_without_delegable_subtree(check, rep, "boxed jobs-env differential"):
                return
            outcomes: dict[str, Outcome] = {}
            observed: dict[str, str | None] = {}
            for name, command in (("py", py), ("rs", rs)):
                output = root / f"{label}-{name}.txt"
                args = [
                    "run",
                    "--dag",
                    str(dag_path),
                    "-s1",
                    f"-j{max_cpus}",
                    "-q",
                    NOPROF,
                    NOFB,
                ]
                if not boxed:
                    args.append("--unsafe-no-cgroups")
                env = {
                    "DAGRUN_JOBS_ENV": "CARGO_BUILD_JOBS",
                    "OBSERVED_PATH": str(output),
                    "DAGRUN_NO_STEP_LOGS": "1",
                }
                if boxed:
                    env.update(
                        {
                            "DAGRUN_FORCE_SCOPE_ATTEMPT": "1",
                            "CARGO_BUILD_JOBS": "8",
                        }
                    )
                outcomes[name] = run(
                    command,
                    args,
                    env,
                    inherit_runner_authority=boxed,
                )
                observed[name] = output.read_text(encoding="utf-8") if output.exists() else None

            if boxed and all(_boxing_capability_unavailable(out) for out in outcomes.values()):
                print(
                    "cross[dagrun]: SKIP boxed jobs-env differential: "
                    "cgroup-v2 + a working systemd --user scope are unavailable"
                )
                rep.skip(
                    check,
                    "boxing",
                    "cgroup-v2 + a working systemd --user scope are unavailable",
                )
            elif boxed and (unboxed := _engines_that_did_not_box(outcomes)):
                rep.bad(
                    check,
                    f"a boxed leg must run boxed; no {_BOXING_ACTIVE_BANNER!r} from {unboxed}: "
                    f"outcomes={outcomes} observed={observed}",
                )
            elif (
                all(out.returncode == 0 for out in outcomes.values())
                and observed == {"py": expected, "rs": expected}
            ):
                rep.ok(check)
            else:
                rep.bad(
                    check,
                    f"expected child width {expected}; outcomes={outcomes} observed={observed}",
                )

        width_leg("unboxed-narrow", 1, "1", boxed=False)
        width_leg("unboxed-roomy", 8, "4", boxed=False)
        width_leg("boxed-narrow", 1, "1", boxed=True)

        def readonly_leg(label: str, channel: str = "BASHOPTS", *, boxed: bool) -> None:
            check = f"jobs-env:{label}"
            if boxed and _skip_without_delegable_subtree(
                check, rep, "boxed readonly jobs-env differential"
            ):
                return
            outcomes: dict[str, Outcome] = {}
            spawned: dict[str, bool] = {}
            for name, command in (("py", py), ("rs", rs)):
                output = root / f"{label}-{name}.txt"
                args = [
                    "run",
                    "--dag",
                    str(dag_path),
                    "-s1",
                    "-j1",
                    "-q",
                    NOPROF,
                    NOFB,
                ]
                if not boxed:
                    args.append("--unsafe-no-cgroups")
                env = {
                    "DAGRUN_JOBS_ENV": channel,
                    "OBSERVED_PATH": str(output),
                    "DAGRUN_NO_STEP_LOGS": "1",
                }
                if boxed:
                    env["DAGRUN_FORCE_SCOPE_ATTEMPT"] = "1"
                outcomes[name] = run(
                    command,
                    args,
                    env,
                    inherit_runner_authority=boxed,
                )
                spawned[name] = output.exists()

            if boxed and all(_boxing_capability_unavailable(out) for out in outcomes.values()):
                print(
                    "cross[dagrun]: SKIP boxed readonly jobs-env differential: "
                    "cgroup-v2 + a working systemd --user scope are unavailable"
                )
                rep.skip(
                    check,
                    "boxing",
                    "cgroup-v2 + a working systemd --user scope are unavailable",
                )
                return
            if boxed and (unboxed := _engines_that_did_not_box(outcomes)):
                rep.bad(
                    check,
                    f"a boxed leg must run boxed; no {_BOXING_ACTIVE_BANNER!r} from {unboxed}: "
                    f"outcomes={outcomes} spawned={spawned}",
                )
                return
            combined = {name: out.stdout + out.stderr for name, out in outcomes.items()}
            if (
                all(out.returncode != 0 for out in outcomes.values())
                and not any(spawned.values())
                and all("did not retain assigned width 1" in text for text in combined.values())
            ):
                rep.ok(check)
            else:
                rep.bad(
                    check,
                    f"shell-managed {channel} must fail before the guest command; "
                    f"outcomes={outcomes} spawned={spawned}",
                )

        readonly_leg("unboxed-readonly", boxed=False)
        readonly_leg("unboxed-dynamic", "LINENO", boxed=False)
        readonly_leg("unboxed-fresh-bash", "IFS", boxed=False)
        readonly_leg("boxed-readonly", boxed=True)

        for leg, extra, phrase in (
            ("missing", {}, "no width channel"),
            (
                "malformed",
                {"DAGRUN_JOBS_ENV": "NOT=A=NAME"},
                "valid environment variable name",
            ),
            *(
                (f"startup-control-{name.lower()}", {"DAGRUN_JOBS_ENV": name},
                 "shell startup/control variable")
                for name in (
                    "BASH_COMPAT", "BASH_ENV", "BASH_XTRACEFD", "CDPATH", "ENV",
                    "EXECIGNORE", "GLOBIGNORE", "PATH", "POSIXLY_CORRECT",
                )
            ),
            (
                "retired-name-ignored",
                {"SAFE_CI_DAG_RUNNER_JOBS_ENV": "CARGO_BUILD_JOBS"},
                "no width channel",
            ),
        ):
            outcomes: dict[str, Outcome] = {}
            spawned: dict[str, bool] = {}
            for name, command in (("py", py), ("rs", rs)):
                output = root / f"{leg}-{name}.txt"
                env = {"OBSERVED_PATH": str(output), "DAGRUN_NO_STEP_LOGS": "1", **extra}
                outcomes[name] = run(
                    command,
                    (
                        "run",
                        "--dag",
                        str(dag_path),
                        "-s1",
                        "-j1",
                        "--unsafe-no-cgroups",
                        "-q",
                        NOPROF,
                        NOFB,
                    ),
                    env,
                )
                spawned[name] = output.exists()
            combined = {name: out.stdout + out.stderr for name, out in outcomes.items()}
            if (
                all(out.returncode == 2 for out in outcomes.values())
                and not any(spawned.values())
                and all(phrase in text for text in combined.values())
            ):
                rep.ok(f"jobs-env:{leg}")
            else:
                rep.bad(
                    f"jobs-env:{leg}",
                    f"expected pre-spawn refusal containing {phrase!r}; "
                    f"outcomes={outcomes} spawned={spawned}",
                )


def _boxed_cpu_bandwidth_case(
    validation_jobs: int, *, dag_path: str, delegated: bool
) -> tuple[dict[str, object], tuple[str, ...], CpuFootprintFacts, tuple[float, ...]]:
    """Build the width-scaled live fixture and the evidence it must produce."""

    if validation_jobs < 1:
        raise ValueError("validation_jobs must be positive")
    dag = _cpu_guest_dag(
        (validation_jobs, validation_jobs),
        duration_s=1.75,
        # At top level the shared run scope is one level above a step. Under the validation
        # graph, the nested scheduler's run root adds one level and the parent-owned delegated
        # step is the shared aggregate bandwidth boundary.
        cgroup_parent_levels=2 if delegated else 1,
        barrier_participants=2,
    )
    args = (
        "run",
        "--dag",
        dag_path,
        "-s2",
        f"-j{validation_jobs}",
        "-q",
        NOPROF,
        NOFB,
    )
    facts = CpuFootprintFacts(
        completed_steps=2,
        workers_per_step=(validation_jobs, validation_jobs),
        max_live_steps=2,
        max_live_workers=validation_jobs * 2,
    )
    return dag, args, facts, (float(validation_jobs),)


def compare_boxed_cpu_bandwidth(
    py: list[str], rs: list[str], rep: Report, *, validation_jobs: int = DEFAULT_VALIDATION_JOBS
) -> None:
    """Anchor ``-j`` / ``--max-cpus`` to live quota and aggregate CPU counters."""

    if _parent_offers_only_unboxed_delegation():
        print(
            "cross[dagrun]: SKIP boxed CPU-bandwidth differential: "
            "the outer scheduler has no cgroup subtree to delegate"
        )
        rep.skip(
            "boxed-cpu-bandwidth",
            "boxing",
            "the outer scheduler has no cgroup subtree to delegate",
        )
        return

    with tempfile.TemporaryDirectory(prefix="dagrun-cross-boxed-cpu-") as td:
        dag_path = os.path.join(td, "dag.json")
        # Each runner-controlled step receives the admitted per-step ceiling J. Both steps must be
        # live together under -s2, so their 2J requested workers exceed J in aggregate while the
        # parent cgroup's long-window CPU bandwidth remains bounded to J core-equivalents.
        dag, args, expected_facts, expected_quota = _boxed_cpu_bandwidth_case(
            validation_jobs,
            dag_path=dag_path,
            delegated=_inside_delegated_harness(),
        )
        Path(dag_path).write_text(json.dumps(dag), encoding="utf-8")
        logs = {name: os.path.join(td, f"boxed-{name}.jsonl") for name in ("py", "rs")}
        extra = {
            name: {
                "CPU_FOOTPRINT_OUTPUT": path,
                "CPU_FOOTPRINT_BARRIER": os.path.join(td, f"boxed-{name}.barrier"),
                "DAGRUN_NO_STEP_LOGS": "1",
                "DAGRUN_FORCE_SCOPE_ATTEMPT": "1",
            }
            for name, path in logs.items()
        }
        outcomes = {
            "py": run(
                py,
                args,
                extra["py"],
                inherit_runner_authority=True,
            ),
            "rs": run(
                rs,
                args,
                extra["rs"],
                inherit_runner_authority=True,
            ),
        }
        unavailable = {name: _boxing_capability_unavailable(out) for name, out in outcomes.items()}
        if all(unavailable.values()):
            print(
                "cross[dagrun]: SKIP boxed CPU-bandwidth differential: "
                "cgroup-v2 + a working systemd --user scope are unavailable"
            )
            rep.skip(
                "boxed-cpu-bandwidth",
                "boxing",
                "cgroup-v2 + a working systemd --user scope are unavailable",
            )
            return
        if any(unavailable.values()) or any(out.returncode != 0 for out in outcomes.values()):
            rep.bad("boxed-cpu-bandwidth", f"py={outcomes['py']}\nrs={outcomes['rs']}")
            return

        normalized: dict[str, tuple[bool, ...]] = {}
        details: dict[str, object] = {}
        try:
            for name, path in logs.items():
                events = load_cpu_events([Path(path)])
                stats = analyze_cpu_footprint(events, bucket_ns=100_000_000)
                facts = _cpu_facts(path)[1]
                limits = check_cpu_limits(stats, max_steps=2)
                bandwidth = check_cgroup_bandwidth(
                    events, min_window_periods=10, scheduler_slack_usec=100_000
                )
                normalized[name] = (
                    facts == expected_facts,
                    limits.ok,
                    bandwidth.checkable,
                    bandwidth.ok,
                    bandwidth.quota_cores == expected_quota,
                )
                details[name] = {
                    "facts": facts,
                    "quota_cores": bandwidth.quota_cores,
                    "checked_windows": bandwidth.checked_windows,
                    "bandwidth_violations": bandwidth.violations,
                    # sampled_cpu_union is intentionally absent: migration/CPU identity is not
                    # an aggregate CPU-bandwidth or simultaneous-worker invariant.
                }
        except (OSError, ValueError) as exc:
            rep.bad("boxed-cpu-bandwidth", f"could not analyze boxed guest evidence: {exc}")
            return
        expected = (True,) * 5
        if normalized.get("py") != normalized.get("rs"):
            rep.bad("boxed-cpu-bandwidth", f"normalized={normalized}; details={details}")
        elif normalized.get("py") != expected:
            rep.bad("boxed-cpu-bandwidth", f"invariants={normalized}; details={details}")
        else:
            rep.ok("boxed-cpu-bandwidth")


def compare_pin_run(py: list[str], rs: list[str], rep: Report) -> None:
    """Exercise shared reservation semantics through the paired safe-runner wrapper."""
    with tempfile.TemporaryDirectory(prefix="dagrun-cross-pin-") as td:
        extra = {"DAGRUN_CORE_LEDGER": os.path.join(td, "ledger.json")}
        missing = ("pin-run", "--cores", "1")
        po, ro = run(py, missing, extra), run(rs, missing, extra)
        if po.returncode == ro.returncode == 2:
            rep.ok("pin-run:missing-command")
        else:
            rep.bad("pin-run:missing-command", f"py={po}\nrs={ro}")
        invalid = ("pin-run", "--cores", "0", "--", "true")
        po, ro = run(py, invalid, extra), run(rs, invalid, extra)
        if po.returncode == ro.returncode == 2 and "must be >= 1" in po.stderr and "must be >= 1" in ro.stderr:
            rep.ok("pin-run:nonpositive-refused")
        else:
            rep.bad("pin-run:nonpositive-refused", f"py={po}\nrs={ro}")
        delegated = _inside_delegated_harness()
        valid = ("pin-run", "--cores", "1", "--", "true")
        if delegated:
            # pin-run currently creates a transient systemd scope directly; unlike `run`, it has
            # no delegated-root execution path.  Invoking that from a parent-owned validation
            # cgroup could migrate the command into a sibling scope and escape the outer step's
            # ownership.  Its live cross-engine HARD-pin behavior remains covered by standalone
            # differential runs on a capable host; do not weaken containment to repeat it here.
            print(
                "cross[dagrun]: SKIP pin-run live execution: the differential is itself "
                "inside a parent-owned delegated cgroup; tracked by #28 delegated-cpuset"
            )
            for label in ("pin-run:reserve-apply-release", "pin-run:signal-status"):
                rep.skip(
                    label,
                    "delegated-live-scope",
                    "the differential is itself inside a parent-owned delegated cgroup",
                )
        else:
            po, ro = run(py, valid, extra), run(rs, valid, extra)
            if po.returncode == ro.returncode == 0:
                rep.ok("pin-run:reserve-apply-release")
            elif (
                po.returncode == ro.returncode == 3
                and "HARD" in po.stderr
                and "HARD" in ro.stderr
            ):
                # The identical refusal is a real parity check. The pin itself never ran.
                rep.ok("pin-run:hard-capability-unavailable-refused")
                rep.skip(
                    "pin-run:reserve-apply-release",
                    "hard-cpuset",
                    "both engines refused the HARD pin with exit 3",
                )
            else:
                rep.bad(
                    "pin-run:reserve-apply-release",
                    f"expected both 0; py={po.returncode} rs={ro.returncode}\n"
                    f"py:{po.stderr}\nrs:{ro.stderr}",
                )

        missing_exec = ("pin-run", "--cores", "1", "--", "/definitely/missing/command")
        po, ro = run(py, missing_exec, extra), run(rs, missing_exec, extra)
        if po.returncode == ro.returncode == 3 and "Traceback" not in po.stderr and "panicked" not in ro.stderr:
            rep.ok("pin-run:missing-executable-clean")
        else:
            rep.bad("pin-run:missing-executable-clean", f"py={po}\nrs={ro}")

        signaled = (
            "pin-run",
            "--cores",
            "1",
            "--",
            sys.executable,
            "-c",
            "import os; os.kill(os.getpid(), 15)",
        )
        if not delegated:
            po, ro = run(py, signaled, extra), run(rs, signaled, extra)
            if po.returncode == ro.returncode == 143:
                rep.ok("pin-run:signal-status")
            elif (
                po.returncode == ro.returncode == 3
                and "HARD" in po.stderr
                and "HARD" in ro.stderr
            ):
                rep.ok("pin-run:signal-status-hard-capability-unavailable")
                rep.skip(
                    "pin-run:signal-status",
                    "hard-cpuset",
                    "both engines refused the HARD pin with exit 3",
                )
            else:
                rep.bad("pin-run:signal-status", f"py={po}\nrs={ro}")


def compare_dagrun(
    rand_count: int,
    seed: int,
    *,
    validation_jobs: int = DEFAULT_VALIDATION_JOBS,
    allowed_skips: frozenset[str] = frozenset(),
) -> int:
    tool = "dagrun"
    py = py_command()
    rs = rs_command(tool)
    rep = Report()

    compare_invocations(py, rs, rep)
    compare_package_guides(tool, py, rs, rep)
    compare_cli_schema(py, rs, rep)
    fixtures = representative_fixtures() + randomized_fixtures(rand_count, seed)
    for fx in fixtures:
        compare_fixture(py, rs, fx, rep)
    examples = example_fixtures()
    for fx in examples:
        compare_example_static(py, rs, fx, rep)
    compare_yaml_isomorphism(py, rs, rep)
    compare_include_composition(py, rs, rep)
    compare_delegated_children_schema(py, rs, rep)
    compare_scalar_parity(py, rs, rep)
    compare_selected_behavior(py, rs, rep)
    compare_uncarried_config_keys(py, rs, rep)
    compare_loader_graph_validation(py, rs, rep)
    compare_uncontained_cpu_budget_notice(py, rs, rep)
    compare_escapee_teardown(py, rs, rep)
    compare_term_attribution(py, rs, rep)
    compare_test_attribution_evidence(py, rs, rep)
    compare_step_log_ceiling(py, rs, rep)
    compare_capture_ceiling(py, rs, rep)
    compare_box_subcommand(py, rs, rep)
    compare_memory_admission(py, rs, rep)
    compare_batch_teardown_grace(py, rs, rep)
    compare_run_timeout(py, rs, rep)
    compare_spawn_failure(py, rs, rep)
    compare_profile_store(py, rs, rep)
    compare_mixed_profile_writers(py, rs, rep)
    compare_profile_run_identity(py, rs, rep)
    compare_profile_timeseries_trace(py, rs, rep)
    compare_plan_feedback(py, rs, rep)
    compare_hostile_numeric_cells(py, rs, rep)
    compare_speedup_model(py, rs, rep)
    compare_scaling_model_sidecar(py, rs, rep)
    compare_cpa_planner(py, rs, rep)
    compare_memory_hardening(py, rs, rep)
    compare_memory_feedback(py, rs, rep)
    compare_summary_sync(py, rs, rep)
    compare_mixed_local_summary_publishers(py, rs, rep)
    compare_sweep_success(py, rs, rep)
    compare_sweep_errors(py, rs, rep)
    compare_sweep_report_collisions(py, rs, rep)
    compare_args_stress(py, rs, rep)
    compare_run_parallel_limits(py, rs, rep)
    compare_boxed_cpu_bandwidth(py, rs, rep, validation_jobs=validation_jobs)
    compare_operator_build_width(py, rs, rep)
    compare_jobs_env_width(py, rs, rep)
    compare_pin_run(py, rs, rep)
    yaml_paths = yaml_fixture_paths()
    n_fixtures = len(fixtures) + len(examples) + len(yaml_paths)

    if rep.failures:
        return divergence_verdict(
            tool,
            rep,
            f"{rep.checks} checks across {n_fixtures} fixtures ({len(examples)} shipped JSON "
            f"examples static-only; {len(yaml_paths)} YAML fixtures for isomorphism)",
            allowed_skips,
        )

    return coverage_verdict(
        tool,
        rep,
        f"{rep.checks} checks across {n_fixtures} fixtures agree "
        f"({len(examples)} shipped JSON examples static-only; "
        f"{len(yaml_paths)} YAML fixtures isomorphic to JSON; "
        f"json byte-identical: {rep.json_byte_identical}, yaml isomorphic: {rep.yaml_isomorphic}, "
        f"scalar-resolution parity cases: {rep.scalar_parity})",
        allowed_skips,
    )


def compare_herdr_run(
    python_command: Sequence[str],
    rust_command: Sequence[str],
    *,
    allowed_skips: frozenset[str] = frozenset(),
) -> int:
    """Run the herdr-run differential and end it with this harness's own verdict.

    ``cross/herdr_differential.py`` keeps its own checks; reporting them here means a herdr-run
    check that could not run (its end-to-end group without a C compiler, say) is listed, counted,
    and allowed or refused exactly like a skipped check of any other tool.
    """
    tool = "herdr-run"
    herdr = build_herdr_run_report(python_command, rust_command)
    for note in herdr.notes:
        print(f"NOTE [{tool}] {note}")
    rep = Report(checks=herdr.checks, failures=list(herdr.failures))
    for label, kind, reason in herdr.skipped:
        rep.skip(label, kind, reason)
    summary = f"{rep.checks} paired checks"
    if rep.failures:
        return divergence_verdict(tool, rep, summary, allowed_skips)
    return coverage_verdict(tool, rep, f"{summary} agree", allowed_skips)


def py_command_for(tool: str) -> list[str]:
    modules = {
        "dagrun": "dagrun",
        "cpuset-alloc": "dagrun.cpuset_allocator",
        "tick-hub": "tick_hub",
        "pr-landing-planner": "pr_landing_planner",
        "herdr-run": "herdr_run",
        "herdr-agent": "agentctl.legacy_cli",
        "agentctl": "agentctl",
    }
    module = modules.get(tool)
    if module is None:
        raise ValueError(f"unknown Python tool {tool!r}")
    return [sys.executable, "-m", module]


def _record_exact(
    rep: Report,
    label: str,
    py: Sequence[str],
    rs: Sequence[str],
    args: Sequence[str],
    extra_env: Mapping[str, str] | None = None,
    *,
    expected: int | None = None,
    compare_stderr: bool = False,
) -> tuple[Outcome, Outcome]:
    po = run(py, args, extra_env)
    ro = run(rs, args, extra_env)
    if po.returncode != ro.returncode:
        rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}\npy:{po.stderr}\nrs:{ro.stderr}")
    elif expected is not None and po.returncode != expected:
        rep.bad(label, f"expected exit {expected}; both returned {po.returncode}")
    elif po.stdout != ro.stdout:
        rep.bad(label, f"stdout differs\n--- py ---\n{po.stdout}\n--- rs ---\n{ro.stdout}")
    elif compare_stderr and po.stderr != ro.stderr:
        rep.bad(label, f"stderr differs\n--- py ---\n{po.stderr}\n--- rs ---\n{ro.stderr}")
    else:
        rep.ok(label)
    return po, ro


def _record_same_exit(
    rep: Report,
    label: str,
    py: Sequence[str],
    rs: Sequence[str],
    args: Sequence[str],
    expected: int | None = None,
) -> tuple[Outcome, Outcome]:
    po = run(py, args)
    ro = run(rs, args)
    if po.returncode != ro.returncode:
        rep.bad(label, f"exit py={po.returncode} rs={ro.returncode}\npy:{po.stderr}\nrs:{ro.stderr}")
    elif expected is not None and po.returncode != expected:
        rep.bad(label, f"expected exit {expected}; both returned {po.returncode}")
    else:
        rep.ok(label)
    return po, ro


def _parsed_json(text_value: str) -> tuple[bool, object]:
    try:
        value: object = json.loads(text_value)
    except json.JSONDecodeError as exc:
        return False, str(exc)
    return True, value


#: The verdicts ``cpuset-alloc selftest`` prints, each with the exit status it must carry.
_SELFTEST_EXIT_BY_VERDICT: dict[str, int] = {
    "HARD": 0,
    "SOFT_OR_INERT": 1,
    "UNTESTABLE": 3,
    "ERROR": 3,
}

#: The reasons ``selftest`` gives for UNTESTABLE, and the skip kind each one stands for. In both
#: cases the mutation probe never ran.
_SELFTEST_UNTESTABLE_KINDS: dict[str, str] = {
    "systemd-run --user --scope unavailable": "boxing",
    "no core outside the reserved set": "multi-cpu",
}


def _record_selftest_verdict(rep: Report, po: Outcome, ro: Outcome) -> None:
    """Record ``selftest:mutation-verdict`` from both engines' ``selftest`` outcomes.

    HARD and SOFT_OR_INERT come from a probe that ran, so agreement on either is a check. An agreed
    UNTESTABLE is parity of the refusal, recorded under its own label; the mutation check itself
    did not run and is a skip of the kind its reason names. An agreed ERROR means the probe
    produced no result, which is a failure. So is a missing verdict (a reservation that failed
    before the probe prints none), a verdict that does not carry its documented exit status, and
    an UNTESTABLE whose reason differs between the engines or is not recognised.
    """

    label = "selftest:mutation-verdict"
    po_parsed, po_value = _parsed_json(po.stdout)
    ro_parsed, ro_value = _parsed_json(ro.stdout)
    py_result: dict[object, object] = (
        po_value if po_parsed and isinstance(po_value, dict) else {}
    )
    rs_result: dict[object, object] = (
        ro_value if ro_parsed and isinstance(ro_value, dict) else {}
    )
    verdict = py_result.get("verdict")
    detail = f"py={po.returncode}:{po_value!r}; rs={ro.returncode}:{ro_value!r}"
    if (
        po.returncode != ro.returncode
        or verdict != rs_result.get("verdict")
        or not isinstance(verdict, str)
        or _SELFTEST_EXIT_BY_VERDICT.get(verdict) != po.returncode
    ):
        rep.bad(label, detail)
    elif verdict in ("HARD", "SOFT_OR_INERT"):
        rep.ok(label)
    elif verdict == "ERROR":
        rep.bad(label, f"both engines reported ERROR: the mutation probe gave no result; {detail}")
    else:
        reason = py_result.get("reason")
        kind = _SELFTEST_UNTESTABLE_KINDS.get(reason) if isinstance(reason, str) else None
        if kind is None or reason != rs_result.get("reason"):
            rep.bad(label, f"UNTESTABLE with a differing or unrecognised reason; {detail}")
        else:
            rep.ok(f"{label}-untestable")
            rep.skip(label, kind, f"both engines reported UNTESTABLE: {reason}")


def _transient_user_bus_refusal(*outcomes: Outcome) -> bool:
    """Return whether a child failed before launch on the systemd user-bus transport.

    A successful capability probe cannot make the later ``systemd-run`` connection atomic: under
    heavy concurrent scope churn the user bus can briefly refuse that final connection.  Keep the
    classification deliberately narrow so an allocator failure, a different transport error, or a
    wrapped command that merely exits nonzero is never retried as infrastructure noise.
    """

    failures = [outcome for outcome in outcomes if outcome.returncode != 0]
    return bool(failures) and all(
        "Failed to connect to user scope bus" in outcome.stderr
        and "Connection refused" in outcome.stderr
        for outcome in failures
    )


def compare_package_guides(
    tool: str, py: Sequence[str], rs: Sequence[str], rep: Report
) -> None:
    """Installed guides may contain language-specific install/API sections, but never leak the
    other distribution's package manager or language."""
    pairs = (
        ("py", py, ("cargo", "crates.io", "rust api", "rs/")),
        ("rs", rs, ("pip install", "pypi", "python api", "py/")),
    )
    for language, command, forbidden in pairs:
        outcome = run(command, ("--userguide",))
        lowered = outcome.stdout.lower()
        checked = lowered
        if tool == "dagrun" and language == "py":
            for value in ("cargo-build", "cargo-test", "cargo-nextest"):
                checked = checked.replace(value, " " * len(value))
        leaks = [token for token in forbidden if token in checked]
        if outcome.returncode != 0:
            rep.bad(f"guide:{language}", f"exit={outcome.returncode}: {outcome.stderr}")
        elif tool not in lowered or len(outcome.stdout) < 500:
            rep.bad(f"guide:{language}", "guide is missing, truncated, or for the wrong tool")
        elif leaks:
            rep.bad(f"guide:{language}", f"cross-language/package-manager references: {leaks}")
        else:
            rep.ok(f"guide:{language}")


def compare_cpuset_alloc(*, allowed_skips: frozenset[str] = frozenset()) -> int:
    tool = "cpuset-alloc"
    py = py_command_for(tool)
    rs = rs_command(tool)
    rep = Report()
    live_scope_allowed = not _inside_delegated_harness()
    if not live_scope_allowed:
        print(
            "cross[cpuset-alloc]: SKIP live systemd-scope cases: the differential is itself "
            "inside a parent-owned delegated cgroup; tracked by #28 delegated-cpuset"
        )
        for label in (
            "selftest:mutation-verdict",
            "interop:py-then-rs",
            "interop:rs-then-py",
            "run:wrapped-help-passthrough",
            "run:signal-status",
        ):
            rep.skip(
                label,
                "delegated-live-scope",
                "the differential is itself inside a parent-owned delegated cgroup",
            )

    _record_exact(rep, "version", py, rs, ("--version",))
    for args, required in (
        ((), ("run", "status", "reclaim", "selftest")),
        (("--help",), ("run", "status", "reclaim", "selftest")),
        (("run", "--help"), ("--cores", "--tag", "--sample-s", "--max-irq-rate")),
        (("status", "--help"), ("--ledger",)),
        (("reclaim", "--help"), ("--ledger",)),
        (("selftest", "--help"), ("--cores", "--sample-s", "--max-irq-rate")),
    ):
        for engine, command in (("py", py), ("rs", rs)):
            outcome = run(command, args)
            missing = [token for token in required if token not in outcome.stdout]
            if outcome.returncode != 0 or missing:
                rep.bad(
                    f"schema:{engine}/{' '.join(args) or 'no-args'}",
                    f"exit={outcome.returncode}; missing={missing}\n{outcome.stdout}\n{outcome.stderr}",
                )
            else:
                rep.ok(f"schema:{engine}/{' '.join(args) or 'no-args'}")

    invalid_budget_cases: tuple[tuple[str, ...], ...] = (
        (
            "run",
            "--cores",
            "1",
            "--sample-s",
            "0",
            "--max-irq-rate",
            "1",
            "--",
            "true",
        ),
        ("selftest", "--sample-s", "0", "--max-irq-rate", "1"),
    )
    for budget_args in invalid_budget_cases:
        po, ro = run(py, budget_args), run(rs, budget_args)
        detail = "--sample-s must be > 0 when --max-irq-rate is set"
        if po.returncode == ro.returncode == 2 and detail in po.stderr and detail in ro.stderr:
            rep.ok(f"irq-budget:nonzero-sample:{budget_args[0]}")
        else:
            rep.bad(f"irq-budget:nonzero-sample:{budget_args[0]}", f"py={po}\nrs={ro}")

    if live_scope_allowed:
        selftest_args = ("selftest", "--cores", "1", "--sample-s", "0")
        _record_selftest_verdict(rep, run(py, selftest_args), run(rs, selftest_args))

    with tempfile.TemporaryDirectory(prefix="cpuset-cross-") as tmp:
        py_ledger = os.path.join(tmp, "py.json")
        rs_ledger = os.path.join(tmp, "rs.json")
        for subcommand in ("status", "reclaim"):
            po = run(py, (subcommand, "--ledger", py_ledger))
            ro = run(rs, (subcommand, "--ledger", rs_ledger))
            p_ok, p_value = _parsed_json(po.stdout)
            r_ok, r_value = _parsed_json(ro.stdout)
            if po.returncode == ro.returncode == 0 and p_ok and r_ok and p_value == r_value:
                rep.ok(f"{subcommand}:empty-ledger")
            else:
                rep.bad(
                    f"{subcommand}:empty-ledger",
                    f"py={po.returncode}:{p_value!r}; rs={ro.returncode}:{r_value!r}",
                )

        Path(py_ledger).write_text("{", encoding="utf-8")
        Path(rs_ledger).write_text("{", encoding="utf-8")
        po = run(py, ("status", "--ledger", py_ledger))
        ro = run(rs, ("status", "--ledger", rs_ledger))
        if (
            po.returncode == ro.returncode == 3
            and "corrupt" in po.stderr
            and "corrupt" in ro.stderr
        ):
            rep.ok("status:corrupt-ledger-fails-closed")
        else:
            rep.bad("status:corrupt-ledger-fails-closed", f"py={po}\nrs={ro}")

        fifo_outcomes: dict[str, Outcome] = {}
        for engine, command in (("py", py), ("rs", rs)):
            fifo_ledger = os.path.join(tmp, f"{engine}-fifo.json")
            os.mkfifo(fifo_ledger, mode=0o600)
            fifo_outcomes[engine] = run(command, ("status", "--ledger", fifo_ledger))
        if all(out.returncode == 3 and out.elapsed_s < 2.0 for out in fifo_outcomes.values()):
            rep.ok("status:fifo-ledger-nonblocking-refusal")
        else:
            rep.bad("status:fifo-ledger-nonblocking-refusal", repr(fifo_outcomes))

        base_record: dict[str, object] = {
            "pid": 1,
            "starttime": 1,
            "cores": [0],
            "tag": "holder",
            "ts": 1.0,
        }

        def changed(**fields: object) -> dict[str, object]:
            return {**base_record, **fields}

        def missing_field(field: str) -> dict[str, object]:
            record = dict(base_record)
            del record[field]
            return record

        invalid_records = {
            "mixed-core": changed(cores=[0, "bad"]),
            "missing-cores": missing_field("cores"),
            "empty-cores": changed(cores=[]),
            "negative-core": changed(cores=[-1]),
            "overflow-core": changed(cores=[1 << 32]),
            "boolean-core": changed(cores=[True]),
            "fractional-core": changed(cores=[1.0]),
            "duplicate-core": changed(cores=[1, 1]),
            "zero-pid": changed(pid=0),
            "overflow-pid": changed(pid=1 << 32),
            "boolean-pid": changed(pid=True),
            "missing-pid": missing_field("pid"),
            "missing-starttime": missing_field("starttime"),
            "zero-starttime": changed(starttime=0),
            "string-starttime": changed(starttime="1"),
            "overflow-starttime": changed(starttime=1 << 64),
            "non-string-tag": changed(tag=7),
            "missing-tag": missing_field("tag"),
            "missing-ts": missing_field("ts"),
            "nonfinite-ts": changed(ts=float("inf")),
            "string-ts": changed(ts="1.0"),
            "boolean-ts": changed(ts=True),
            "negative-ts": changed(ts=-1),
        }
        for case, record in invalid_records.items():
            original = json.dumps({"reservations": [record]})
            outcomes: dict[str, Outcome] = {}
            preserved = True
            for engine, command in (("py", py), ("rs", rs)):
                path = os.path.join(tmp, f"invalid-{case}-{engine}.json")
                Path(path).write_text(original, encoding="utf-8")
                os.chmod(path, 0o600)
                outcomes[engine] = run(command, ("status", "--ledger", path))
                preserved = preserved and Path(path).read_text(encoding="utf-8") == original
            if all(outcome.returncode == 3 for outcome in outcomes.values()) and preserved:
                rep.ok(f"schema:{case}-fails-closed")
            else:
                rep.bad(
                    f"schema:{case}-fails-closed",
                    f"preserved={preserved}; outcomes={outcomes!r}",
                )

        # A parent-owned record is live from both child processes' perspective.
        stat_text = open(f"/proc/{os.getpid()}/stat", encoding="utf-8").read()
        starttime = int(stat_text[stat_text.rfind(")") + 2 :].split()[19])
        live_payload = {
            "reservations": [
                {
                    "pid": os.getpid(),
                    "starttime": starttime,
                    "cores": [7, 3],
                    "tag": "cross-live",
                    "ts": 1.25,
                }
            ]
        }
        for path in (py_ledger, rs_ledger):
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(live_payload, handle)
        po = run(py, ("status", "--ledger", py_ledger))
        ro = run(rs, ("status", "--ledger", rs_ledger))
        p_ok, p_value = _parsed_json(po.stdout)
        r_ok, r_value = _parsed_json(ro.stdout)
        if po.returncode == ro.returncode == 0 and p_ok and r_ok and p_value == r_value:
            rep.ok("status:live-shared-schema")
        else:
            rep.bad("status:live-shared-schema", f"py={p_value!r}; rs={r_value!r}")

        dead_payload = {
            "reservations": [
                {"pid": 2147483647, "starttime": 1, "cores": [5], "tag": "dead", "ts": 2.5}
            ]
        }
        for path in (py_ledger, rs_ledger):
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(dead_payload, handle)
        po = run(py, ("reclaim", "--ledger", py_ledger))
        ro = run(rs, ("reclaim", "--ledger", rs_ledger))
        p_ok, p_value = _parsed_json(po.stdout)
        r_ok, r_value = _parsed_json(ro.stdout)
        if po.returncode == ro.returncode == 0 and p_ok and r_ok and p_value == r_value:
            rep.ok("reclaim:dead-shared-schema")
        else:
            rep.bad("reclaim:dead-shared-schema", f"py={p_value!r}; rs={r_value!r}")

        # Each implementation must be able to observe the other's LIVE reservation, choose a
        # disjoint core, and leave the shared ledger empty after both wrapped commands exit.
        if live_scope_allowed and len(os.sched_getaffinity(0)) >= 2:
            assignment_re = re.compile(r'reserved (\{"cores":\[[^]]*\],"count":\d+\})')

            def reserved_cores(stderr: str) -> set[int] | None:
                match = assignment_re.search(stderr)
                if match is None:
                    return None
                value: object = json.loads(match.group(1))
                if not isinstance(value, dict):
                    return None
                cores = value.get("cores")
                if not isinstance(cores, list) or not all(
                    isinstance(core, int) and not isinstance(core, bool) for core in cores
                ):
                    return None
                return set(cores)

            for label, first, second in (
                ("py-then-rs", py, rs),
                ("rs-then-py", rs, py),
            ):
                hold = (
                    "run",
                    "--cores",
                    "1",
                    "--tag",
                    label,
                    "--",
                    sys.executable,
                    "-c",
                    "import time; time.sleep(2)",
                )
                # Retry the WHOLE pair, never just the second command.  Replaying only one side
                # would test against a different reservation lifetime and could leave the first
                # attempt's held core in the comparison.  Each attempt gets an independent ledger
                # and the first child is reaped before the next begins.
                interop_attempts = 3
                for attempt in range(1, interop_attempts + 1):
                    shared_ledger = os.path.join(tmp, f"interop-{label}-{attempt}.json")
                    extra = {"DAGRUN_CORE_LEDGER": shared_ledger}
                    first_started = time.monotonic()
                    first_proc = subprocess.Popen(
                        [*first, *hold],
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        text=True,
                        env=_env(extra),
                        start_new_session=True,
                    )
                    deadline = time.monotonic() + 10
                    while first_proc.poll() is None and time.monotonic() < deadline:
                        try:
                            payload = json.loads(Path(shared_ledger).read_text(encoding="utf-8"))
                        except (OSError, json.JSONDecodeError):
                            payload = {}
                        if isinstance(payload, dict) and payload.get("reservations"):
                            break
                        time.sleep(0.02)
                    try:
                        second_outcome = run(second, hold, extra)
                    finally:
                        try:
                            first_stdout, first_stderr = first_proc.communicate(timeout=30)
                        except subprocess.TimeoutExpired:
                            first_proc.kill()
                            first_stdout, first_stderr = first_proc.communicate()
                    first_outcome = Outcome(
                        first_proc.returncode or 0,
                        first_stdout,
                        first_stderr,
                        time.monotonic() - first_started,
                    )
                    if not _transient_user_bus_refusal(first_outcome, second_outcome):
                        break
                    if attempt < interop_attempts:
                        print(
                            f"cross[{tool}]: transient systemd user-bus refusal during "
                            f"interop:{label} attempt {attempt}/{interop_attempts}; retrying pair",
                            file=sys.stderr,
                        )
                if first_outcome.returncode == second_outcome.returncode == 0:
                    try:
                        first_cores = reserved_cores(first_outcome.stderr)
                        second_cores = reserved_cores(second_outcome.stderr)
                        final_payload: object = json.loads(
                            Path(shared_ledger).read_text(encoding="utf-8")
                        )
                    except (OSError, TypeError, json.JSONDecodeError):
                        rep.bad(
                            f"interop:{label}",
                            f"could not parse assignments/ledger\nfirst={first_outcome}\n"
                            f"second={second_outcome}",
                        )
                    else:
                        if (
                            first_cores is not None
                            and second_cores is not None
                            and first_cores
                            and second_cores
                            and first_cores.isdisjoint(second_cores)
                            and final_payload == {"reservations": []}
                        ):
                            rep.ok(f"interop:{label}")
                        else:
                            rep.bad(
                                f"interop:{label}",
                                f"first={first_cores}; second={second_cores}; "
                                f"final={final_payload!r}",
                            )
                elif (
                    first_outcome.returncode == second_outcome.returncode == 3
                    and "HARD" in first_outcome.stderr
                    and "HARD" in second_outcome.stderr
                ):
                    rep.ok(f"interop:{label}:hard-capability-unavailable")
                    rep.skip(
                        f"interop:{label}",
                        "hard-cpuset",
                        "both engines refused the HARD pin with exit 3",
                    )
                else:
                    rep.bad(
                        f"interop:{label}",
                        f"first={first_outcome}\nsecond={second_outcome}",
                    )
        elif live_scope_allowed:
            for label in ("py-then-rs", "rs-then-py"):
                rep.skip(
                    f"interop:{label}",
                    "multi-cpu",
                    "two disjoint reservations need at least two usable CPUs",
                )

    _record_same_exit(rep, "run:missing-command", py, rs, ("run", "--cores", "1"), 2)
    _record_same_exit(
        rep,
        "run:separator-required",
        py,
        rs,
        ("run", "--cores", "1", "true"),
        2,
    )
    invalid_invocations: tuple[tuple[str, tuple[str, ...]], ...] = (
        ("run:nonpositive-cores", ("run", "--cores", "0", "--", "true")),
        (
            "run:negative-sample",
            ("run", "--cores", "1", "--sample-s", "-1", "--", "true"),
        ),
        (
            "run:nonfinite-sample",
            ("run", "--cores", "1", "--sample-s", "nan", "--", "true"),
        ),
        ("selftest:tag-rejected", ("selftest", "--tag", "x")),
    )
    for label, invalid_args in invalid_invocations:
        _record_same_exit(rep, label, py, rs, invalid_args, 2)

    if live_scope_allowed:
        # A wrapped command's own help flag belongs to that command, not the allocator.
        help_args = ("run", "--cores", "1", "--", "printf", "%s\\n", "--help")
        po, ro = run(py, help_args), run(rs, help_args)
        if (
            po.returncode == ro.returncode == 0
            and po.stdout.endswith("--help\n")
            and ro.stdout.endswith("--help\n")
        ):
            rep.ok("run:wrapped-help-passthrough")
        elif (
            po.returncode == ro.returncode == 3
            and "HARD" in po.stderr
            and "HARD" in ro.stderr
        ):
            rep.ok("run:wrapped-help-hard-capability-unavailable")
            rep.skip(
                "run:wrapped-help-passthrough",
                "hard-cpuset",
                "both engines refused the HARD pin with exit 3",
            )
        else:
            rep.bad("run:wrapped-help-passthrough", f"py={po}\nrs={ro}")

    missing_exec = ("run", "--cores", "1", "--", "/definitely/missing/command")
    po, ro = run(py, missing_exec), run(rs, missing_exec)
    if (
        po.returncode == ro.returncode == 3
        and "Traceback" not in po.stderr
        and "panicked" not in ro.stderr
    ):
        rep.ok("run:missing-executable-clean")
    else:
        rep.bad("run:missing-executable-clean", f"py={po}\nrs={ro}")

    signaled = (
        "run",
        "--cores",
        "1",
        "--",
        sys.executable,
        "-c",
        "import os; os.kill(os.getpid(), 15)",
    )
    if live_scope_allowed:
        po, ro = run(py, signaled), run(rs, signaled)
        if po.returncode == ro.returncode == 143:
            rep.ok("run:signal-status")
        elif (
            po.returncode == ro.returncode == 3
            and "HARD" in po.stderr
            and "HARD" in ro.stderr
        ):
            rep.ok("run:signal-status-hard-capability-unavailable")
            rep.skip(
                "run:signal-status",
                "hard-cpuset",
                "both engines refused the HARD pin with exit 3",
            )
        else:
            rep.bad("run:signal-status", f"py={po}\nrs={ro}")
    _record_same_exit(rep, "cli:abbreviation-refused", py, rs, ("status", "--ledg", "x"), 2)
    _record_same_exit(rep, "unknown-command", py, rs, ("not-a-command",), 2)

    if rep.failures:
        return divergence_verdict(tool, rep, f"{rep.checks} checks", allowed_skips)
    return coverage_verdict(
        tool, rep, f"{rep.checks} behavioral and ledger-schema checks agree", allowed_skips
    )


def compare_tick_hub_phased_cadence(
    py: Sequence[str], rs: Sequence[str], rep: Report
) -> None:
    """Phased cadences and per-gate timeouts must mean the same thing in both editions.

    These live HERE, in the mandatory differential, rather than in a pytest case that skips
    when the Rust binary is absent. The normal order runs the Python suite before cargo
    builds anything, so such a case passes by skipping on a clean checkout -- it would have
    reported success for exactly the one-edition regression it exists to catch. This harness
    resolves the binary through the tracked launcher and RAISES when it cannot, so absence
    is a failure rather than a quiet pass.
    """
    phased: dict[str, object] = {
        "reminders": [
            {
                "name": "phase_a",
                "cadence_secs": 3600,
                "cadence_offset_secs": 0,
                "cadence_window_secs": 1800,
                "emit": {"kind": "note", "title": "a"},
            },
            {
                "name": "phase_b",
                "cadence_secs": 3600,
                "cadence_offset_secs": 1800,
                "emit": {"kind": "note", "title": "b"},
            },
            {"name": "plain", "cadence_secs": 3600, "emit": {"kind": "note", "title": "p"}},
        ]
    }
    bounded: dict[str, object] = {
        "reminders": [
            {
                "name": "bounded",
                "cadence_secs": 3600,
                "gate": {
                    "cmd": "true",
                    "when": "failure",
                    "capture": True,
                    "timeout_secs": 90,
                    "parallel": True,
                },
                "emit": {"kind": "note", "title": "b"},
            }
        ]
    }
    # Each must be refused by BOTH editions. A shape one takes and the other rejects splits
    # the contract just as surely as a parse failure, only more quietly.
    refusals: dict[str, dict[str, object]] = {
        "offset_at_cadence": {"reminders": [{"name": "r", "cadence_secs": 3600,
            "cadence_offset_secs": 3600, "emit": {"kind": "note", "title": "t"}}]},
        "offset_on_every_tick": {"reminders": [{"name": "r", "cadence_secs": 0,
            "cadence_offset_secs": 5, "emit": {"kind": "note", "title": "t"}}]},
        "window_without_offset": {"reminders": [{"name": "r", "cadence_secs": 3600,
            "cadence_window_secs": 60, "emit": {"kind": "note", "title": "t"}}]},
        "window_beyond_cadence": {"reminders": [{"name": "r", "cadence_secs": 3600,
            "cadence_offset_secs": 0, "cadence_window_secs": 3601,
            "emit": {"kind": "note", "title": "t"}}]},
        "gate_timeout_zero": {"reminders": [{"name": "r", "cadence_secs": 3600,
            "gate": {"cmd": "true", "timeout_secs": 0},
            "emit": {"kind": "note", "title": "t"}}]},
    }
    with tempfile.TemporaryDirectory(prefix="tick-hub-phase-") as tmp:
        for label, config in (("phased", phased), ("gate-timeout", bounded)):
            path = os.path.join(tmp, f"{label}.json")
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(config, handle, ensure_ascii=False)
            for subcommand in ("list", "json"):
                _record_exact(
                    rep,
                    f"phase:{label}/{subcommand}",
                    py,
                    rs,
                    (subcommand, "--config", path),
                    expected=0,
                )
        for label, refusal in sorted(refusals.items()):
            path = os.path.join(tmp, f"refuse-{label}.json")
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(refusal, handle, ensure_ascii=False)
            _record_exact(
                rep,
                f"phase:refuse/{label}",
                py,
                rs,
                ("list", "--config", path),
                expected=2,
            )


def compare_tick_hub(rand_count: int, seed: int) -> int:
    tool = "tick-hub"
    py = py_command_for(tool)
    rs = rs_command(tool)
    rep = Report()

    _record_exact(rep, "version", py, rs, ("--version",))
    compare_package_guides(tool, py, rs, rep)
    compare_tick_hub_phased_cadence(py, rs, rep)
    for engine, command in (("py", py), ("rs", rs)):
        top = run(command, ("--help",))
        required = ("tick", "state", "list", "json", "yaml", "quickstart", "--userguide")
        missing = [token for token in required if token not in top.stdout]
        if top.returncode != 0 or missing:
            rep.bad(f"schema:{engine}/commands", f"exit={top.returncode}; missing={missing}")
        else:
            rep.ok(f"schema:{engine}/commands")
        command_flags = {
            "tick": (
                "--config",
                "--state",
                "--fired-state",
                "--now",
                "--current-tick-min",
                "--flush",
                "--report-pending",
                "--no-header",
            ),
            "state": ("--state", "--current-tick-min"),
            "list": ("--config",),
            "json": ("--config",),
            "yaml": ("--config",),
        }
        for subcommand, flags in command_flags.items():
            outcome = run(command, (subcommand, "--help"))
            absent = [flag for flag in flags if flag not in outcome.stdout]
            if outcome.returncode != 0 or absent:
                rep.bad(
                    f"schema:{engine}/{subcommand}",
                    f"exit={outcome.returncode}; missing={absent}\n{outcome.stdout}",
                )
            else:
                rep.ok(f"schema:{engine}/{subcommand}")

    with tempfile.TemporaryDirectory(prefix="tick-hub-cross-") as tmp:
        watched = os.path.join(tmp, "snapshot.dat")
        with open(watched, "w", encoding="utf-8") as handle:
            handle.write("snapshot\n")
        os.utime(watched, (800, 800))
        config: dict[str, object] = {
            "description": "cross-check unicode \u2603",
            "reminders": [
                {
                    "name": "always",
                    "cadence_secs": 0,
                    "emit": {
                        "kind": "action",
                        "skill": "refresh-cache",
                        "fields": {"z": "last", "a": "first"},
                        "title": "refresh {a} {z}",
                    },
                },
                {
                    "name": "gated",
                    "cadence_secs": 60,
                    "gate": {
                        "cmd": "printf 'count=3\\n'",
                        "when": "nonempty",
                        "capture": True,
                        "parallel": True,
                    },
                    "emit": {
                        "kind": "action",
                        "skill": "triage",
                        "title": "triage {count}",
                    },
                },
                {
                    "name": "parallel_note",
                    "gate": {"cmd": "true", "when": "success", "parallel": True},
                    "emit": {"kind": "note", "title": "parallel gate completed"},
                },
                {
                    "name": "guarded",
                    "requires_flags": ["ready"],
                    "emit": {"kind": "note", "title": "guard is enabled"},
                },
                {
                    "name": "not_due",
                    "cadence_secs": 500,
                    "emit": {"kind": "note", "title": "must stay quiet"},
                },
            ],
            "health_checks": [
                {
                    "name": "snapshot",
                    "glob": watched,
                    "threshold_secs": 100,
                    "detail": "fixture age",
                },
                {
                    "name": "absent",
                    "glob": os.path.join(tmp, "missing-*"),
                    "threshold_secs": 1,
                },
            ],
        }
        json_path = os.path.join(tmp, "config.json")
        with open(json_path, "w", encoding="utf-8") as handle:
            json.dump(config, handle, ensure_ascii=False)
        state_path = os.path.join(tmp, "state.yaml")
        with open(state_path, "w", encoding="utf-8") as handle:
            handle.write(
                "enabled: true\n"
                "tick_frequency_min: 30\n"
                "label: cross-host\n"
                "flags: {ready: true, count: 2}\n"
                "_annotation: ignored\n"
            )
        fired = "gated=900\nnot_due=900\n"
        py_fired = os.path.join(tmp, "py-fired")
        rs_fired = os.path.join(tmp, "rs-fired")
        for path in (py_fired, rs_fired):
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(fired)

        for subcommand in ("json", "list"):
            _record_exact(rep, f"representative:{subcommand}", py, rs, (subcommand, "--config", json_path))
        _record_exact(
            rep,
            "representative:state",
            py,
            rs,
            ("state", "--state", state_path, "--current-tick-min", "15"),
        )
        common_tick = (
            "tick",
            "--config",
            json_path,
            "--state",
            state_path,
            "--now",
            "1000",
            "--no-header",
        )
        po = run(py, (*common_tick, "--fired-state", py_fired))
        ro = run(rs, (*common_tick, "--fired-state", rs_fired))
        if po.returncode == ro.returncode == 0 and po.stdout == ro.stdout:
            rep.ok("representative:tick")
        else:
            rep.bad(
                "representative:tick",
                f"py={po.returncode}:{po.stdout!r}\nrs={ro.returncode}:{ro.stdout!r}",
            )

        quiet_config = {
            "reminders": [
                {
                    "name": "quiet",
                    "cadence_secs": 3600,
                    "gate": {"cmd": "true", "when": "failure"},
                    "emit": {"skill": "warn", "title": "problem"},
                }
            ]
        }
        quiet_path = os.path.join(tmp, "report-pending.json")
        with open(quiet_path, "w", encoding="utf-8") as handle:
            json.dump(quiet_config, handle)
        py_quiet_fired = os.path.join(tmp, "py-report-pending-fired")
        rs_quiet_fired = os.path.join(tmp, "rs-report-pending-fired")
        quiet_args = (
            "tick",
            "--config",
            quiet_path,
            "--state",
            state_path,
            "--now",
            "1000",
            "--report-pending",
            "--no-header",
        )
        po = run(py, (*quiet_args, "--fired-state", py_quiet_fired, "--flush"))
        ro = run(rs, (*quiet_args, "--fired-state", rs_quiet_fired, "--flush"))
        py_quiet_state = Path(py_quiet_fired).read_text(encoding="utf-8")
        rs_quiet_state = Path(rs_quiet_fired).read_text(encoding="utf-8")
        po_again = run(
            py,
            (
                *quiet_args[:6],
                "1001",
                *quiet_args[7:],
                "--fired-state",
                py_quiet_fired,
            ),
        )
        ro_again = run(
            rs,
            (
                *quiet_args[:6],
                "1001",
                *quiet_args[7:],
                "--fired-state",
                rs_quiet_fired,
            ),
        )
        if (
            po.returncode == ro.returncode == po_again.returncode == ro_again.returncode == 0
            and po.stdout == ro.stdout
            and po_again.stdout == ro_again.stdout
            and py_quiet_state == rs_quiet_state
            and "quiet=1000\n" in py_quiet_state
            and "CLEAN: quiet ran and found nothing to report" in po.stdout
            and "CLEAN: quiet" not in po_again.stdout
        ):
            rep.ok("report-pending:flush-preserves-cadence")
        else:
            rep.bad(
                "report-pending:flush-preserves-cadence",
                f"first py={po}\nfirst rs={ro}\nsecond py={po_again}\nsecond rs={ro_again}\n"
                f"state py={py_quiet_state!r} rs={rs_quiet_state!r}",
            )

        py_broken_fired = os.path.join(tmp, "py-broken-pipe-fired")
        rs_broken_fired = os.path.join(tmp, "rs-broken-pipe-fired")
        broken_args = (
            "tick",
            "--config",
            quiet_path,
            "--state",
            state_path,
            "--now",
            "2000",
            "--report-pending",
            "--no-header",
        )
        po = run_with_closed_stdout(
            py, (*broken_args, "--fired-state", py_broken_fired, "--flush")
        )
        ro = run_with_closed_stdout(
            rs, (*broken_args, "--fired-state", rs_broken_fired, "--flush")
        )
        if (
            po.returncode != 0
            and ro.returncode != 0
            and not os.path.exists(py_broken_fired)
            and not os.path.exists(rs_broken_fired)
        ):
            rep.ok("report-pending:broken-pipe-does-not-consume-cadence")
        else:
            rep.bad(
                "report-pending:broken-pipe-does-not-consume-cadence",
                f"py={po} py_state_exists={os.path.exists(py_broken_fired)}\n"
                f"rs={ro} rs_state_exists={os.path.exists(rs_broken_fired)}",
            )

        ordered_config = {
            "reminders": [
                {
                    "name": "first",
                    "gate": {"cmd": "exit 1", "when": "failure"},
                    "emit": {"skill": "warn", "title": "first found a problem"},
                },
                {
                    "name": "suppressed",
                    "requires_flags": ["disabled"],
                    "emit": {"skill": "warn", "title": "suppressed fired"},
                },
                {
                    "name": "third",
                    "gate": {"cmd": "exit 1", "when": "failure"},
                    "emit": {"skill": "warn", "title": "third found a problem"},
                },
            ]
        }
        ordered_path = os.path.join(tmp, "report-order.json")
        with open(ordered_path, "w", encoding="utf-8") as handle:
            json.dump(ordered_config, handle)
        ordered_args = (
            "tick",
            "--config",
            ordered_path,
            "--state",
            state_path,
            "--now",
            "1000",
            "--report-pending",
            "--no-header",
        )
        po = run(py, ordered_args)
        ro = run(rs, ordered_args)
        ordered_verdicts = tuple(
            line
            for line in po.stdout.splitlines()
            if line.startswith(("ACTION: warn", "SUPPRESSED: "))
        )
        if (
            po.returncode == ro.returncode == 0
            and po.stdout == ro.stdout
            and ordered_verdicts
            == (
                'ACTION: warn title="first found a problem"',
                "SUPPRESSED: suppressed did not run; required flag(s) not set: disabled",
                'ACTION: warn title="third found a problem"',
            )
        ):
            rep.ok("report-pending:suppressed-keeps-config-order")
        else:
            rep.bad(
                "report-pending:suppressed-keeps-config-order",
                f"py={po.returncode}:{po.stdout!r}\nrs={ro.returncode}:{ro.stdout!r}",
            )

        streaming_config = {
            "reminders": [
                {
                    "name": "independent",
                    "gate": {"cmd": "exit 1", "when": "failure"},
                    "emit": {"skill": "warn", "title": "independent found a problem"},
                },
                {
                    "name": "dependent",
                    "depends_on": ["foundation"],
                    "gate": {"cmd": "exit 0", "when": "failure"},
                    "emit": {"skill": "warn", "title": "dependent problem"},
                },
                {
                    "name": "foundation",
                    "gate": {"cmd": "sleep 1; exit 75", "when": "failure"},
                    "emit": {"skill": "warn", "title": "foundation problem"},
                },
            ]
        }
        streaming_path = os.path.join(tmp, "streaming-dependencies.json")
        with open(streaming_path, "w", encoding="utf-8") as handle:
            json.dump(streaming_config, handle)
        streaming_args = (
            "tick",
            "--config",
            streaming_path,
            "--state",
            state_path,
            "--now",
            "1000",
            "--report-pending",
            "--no-header",
        )
        expected_prefix = 'ACTION: warn title="independent found a problem"\n'
        po, py_early, py_before_exit = run_until_stdout_contains(
            py, streaming_args, expected_prefix
        )
        ro, rs_early, rs_before_exit = run_until_stdout_contains(
            rs, streaming_args, expected_prefix
        )
        forbidden_early = b"CLEAN: dependent ran and found nothing to report"
        if (
            po.returncode == ro.returncode == 0
            and po.stdout == ro.stdout
            and py_before_exit
            and rs_before_exit
            and forbidden_early not in py_early
            and forbidden_early not in rs_early
            and "NO_RESULT: dependent is unevaluable" in po.stdout
        ):
            rep.ok("streaming:mixed-dependency-prefix-visible")
        else:
            rep.bad(
                "streaming:mixed-dependency-prefix-visible",
                f"py_before_exit={py_before_exit} py_early={py_early!r} py={po}\n"
                f"rs_before_exit={rs_before_exit} rs_early={rs_early!r} rs={ro}",
            )

        dependency_config = {
            "reminders": [
                {
                    "name": "foundation",
                    "gate": {
                        "cmd": "printf 'summary=foundation unavailable\\n'; exit 75",
                        "when": "failure",
                        "capture": True,
                    },
                    "emit": {"skill": "warn", "title": "foundation problem"},
                },
                {
                    "name": "dependent",
                    "depends_on": ["foundation"],
                    "gate": {"cmd": "exit 0", "when": "failure"},
                    "emit": {"skill": "warn", "title": "dependent problem"},
                },
                {
                    "name": "independent",
                    "gate": {"cmd": "exit 0", "when": "failure"},
                    "emit": {"skill": "warn", "title": "independent problem"},
                },
            ]
        }
        dependency_path = os.path.join(tmp, "dependencies.json")
        with open(dependency_path, "w", encoding="utf-8") as handle:
            json.dump(dependency_config, handle)
        py_dependency_fired = os.path.join(tmp, "py-dependency-fired")
        rs_dependency_fired = os.path.join(tmp, "rs-dependency-fired")
        dependency_args = (
            "tick",
            "--config",
            dependency_path,
            "--state",
            state_path,
            "--now",
            "1000",
            "--no-header",
        )
        po = run(py, (*dependency_args, "--fired-state", py_dependency_fired, "--flush"))
        ro = run(rs, (*dependency_args, "--fired-state", rs_dependency_fired, "--flush"))
        with open(py_dependency_fired, encoding="utf-8") as handle:
            py_dependency_state = handle.read()
        with open(rs_dependency_fired, encoding="utf-8") as handle:
            rs_dependency_state = handle.read()
        if (
            po.returncode == ro.returncode == 0
            and po.stdout == ro.stdout
            and py_dependency_state == rs_dependency_state
            and "NO_RESULT: dependent is unevaluable" in po.stdout
            and not any(
                line.startswith("dependent=") for line in py_dependency_state.splitlines()
            )
            and any(
                line == "independent=1000" for line in py_dependency_state.splitlines()
            )
        ):
            rep.ok("dependency-no-result:quiet-dependent-unevaluable")
        else:
            rep.bad(
                "dependency-no-result:quiet-dependent-unevaluable",
                f"py={po.returncode}:{po.stdout!r}:{py_dependency_state!r}\n"
                f"rs={ro.returncode}:{ro.stdout!r}:{rs_dependency_state!r}",
            )

        unresolved_config: dict[str, object] = {
            "reminders": [
                {
                    "name": "unresolved",
                    "gate": {
                        "cmd": "exit 1",
                        "when": "failure",
                        "capture": True,
                    },
                    "emit": {"skill": "warn", "title": "problem: {summary}"},
                }
            ]
        }
        unresolved_path = os.path.join(tmp, "unresolved.json")
        with open(unresolved_path, "w", encoding="utf-8") as handle:
            json.dump(unresolved_config, handle)
        py_unresolved_fired = os.path.join(tmp, "py-unresolved-fired")
        rs_unresolved_fired = os.path.join(tmp, "rs-unresolved-fired")
        repeated_ok = True
        repeated_detail = []
        for consecutive, now in enumerate((1000, 1001, 1002), start=1):
            unresolved_args = (
                "tick",
                "--config",
                unresolved_path,
                "--state",
                state_path,
                "--now",
                str(now),
                "--no-header",
                "--flush",
            )
            po = run(py, (*unresolved_args, "--fired-state", py_unresolved_fired))
            ro = run(rs, (*unresolved_args, "--fired-state", rs_unresolved_fired))
            with open(py_unresolved_fired, encoding="utf-8") as handle:
                py_unresolved_state = handle.read()
            with open(rs_unresolved_fired, encoding="utf-8") as handle:
                rs_unresolved_state = handle.read()
            repeated = "consecutive_failures=" in po.stdout
            expected_repeated = consecutive >= 3
            step_ok = (
                po.returncode == ro.returncode == 0
                and po.stdout == ro.stdout
                and py_unresolved_state == rs_unresolved_state
                and repeated == expected_repeated
                and "reason=unresolved-placeholder" in po.stdout
                and f".count={consecutive}\n" in py_unresolved_state
                and ".first_failure_epoch=1000\n" in py_unresolved_state
                and not any(
                    line.startswith("unresolved=")
                    for line in py_unresolved_state.splitlines()
                )
            )
            repeated_ok = repeated_ok and step_ok
            repeated_detail.append(
                f"step={consecutive} ok={step_ok} py={po.stdout!r} state={py_unresolved_state!r}"
            )
        if repeated_ok:
            rep.ok("unresolved-render:third-consecutive-escalates")
        else:
            rep.bad(
                "unresolved-render:third-consecutive-escalates",
                "\n".join(repeated_detail),
            )

        recovered_config = {
            "reminders": [
                {
                    "name": "unresolved",
                    "gate": {
                        "cmd": "printf 'summary=recovered\\n'; exit 1",
                        "when": "failure",
                        "capture": True,
                    },
                    "emit": {"skill": "warn", "title": "problem: {summary}"},
                }
            ]
        }
        with open(unresolved_path, "w", encoding="utf-8") as handle:
            json.dump(recovered_config, handle)
        recovery_args = (
            "tick",
            "--config",
            unresolved_path,
            "--state",
            state_path,
            "--now",
            "1003",
            "--no-header",
            "--flush",
        )
        po = run(py, (*recovery_args, "--fired-state", py_unresolved_fired))
        ro = run(rs, (*recovery_args, "--fired-state", rs_unresolved_fired))
        with open(py_unresolved_fired, encoding="utf-8") as handle:
            py_recovered_state = handle.read()
        with open(rs_unresolved_fired, encoding="utf-8") as handle:
            rs_recovered_state = handle.read()
        if (
            po.returncode == ro.returncode == 0
            and po.stdout == ro.stdout
            and py_recovered_state == rs_recovered_state
            and "problem: recovered" in po.stdout
            and "unresolved=1003\n" in py_recovered_state
            and "__tick_hub_internal__.unresolved_render.unresolved" not in py_recovered_state
        ):
            rep.ok("unresolved-render:successful-render-clears-streak")
        else:
            rep.bad(
                "unresolved-render:successful-render-clears-streak",
                f"py={po.returncode}:{po.stdout!r}:{py_recovered_state!r}\n"
                f"rs={ro.returncode}:{ro.stdout!r}:{rs_recovered_state!r}",
            )

        po = run(py, (*common_tick, "--fired-state", py_fired, "--flush"))
        ro = run(rs, (*common_tick, "--fired-state", rs_fired, "--flush"))
        with open(py_fired, encoding="utf-8") as handle:
            py_state = handle.read()
        with open(rs_fired, encoding="utf-8") as handle:
            rs_state = handle.read()
        if po.returncode == ro.returncode == 0 and po.stdout == ro.stdout and py_state == rs_state:
            rep.ok("representative:flush-bytes")
        else:
            rep.bad(
                "representative:flush-bytes",
                f"exit py={po.returncode} rs={ro.returncode}; stdout_equal={po.stdout == ro.stdout}; "
                f"state py={py_state!r} rs={rs_state!r}",
            )

        py_yaml = run(py, ("yaml", "--config", json_path))
        rs_yaml = run(rs, ("yaml", "--config", json_path))
        py_yaml_path = os.path.join(tmp, "py.yaml")
        rs_yaml_path = os.path.join(tmp, "rs.yaml")
        with open(py_yaml_path, "w", encoding="utf-8") as handle:
            handle.write(py_yaml.stdout)
        with open(rs_yaml_path, "w", encoding="utf-8") as handle:
            handle.write(rs_yaml.stdout)
        py_roundtrip = run(py, ("json", "--config", py_yaml_path))
        rs_from_py = run(rs, ("json", "--config", py_yaml_path))
        py_from_rs = run(py, ("json", "--config", rs_yaml_path))
        rs_roundtrip = run(rs, ("json", "--config", rs_yaml_path))
        canonical = run(py, ("json", "--config", json_path)).stdout
        outputs = (
            py_roundtrip.stdout,
            rs_from_py.stdout,
            py_from_rs.stdout,
            rs_roundtrip.stdout,
        )
        if (
            py_yaml.returncode == rs_yaml.returncode == 0
            and all(outcome == canonical for outcome in outputs)
        ):
            rep.ok("yaml:bidirectional-isomorphism")
        else:
            rep.bad("yaml:bidirectional-isomorphism", "YAML emit/load did not preserve canonical JSON")

        invalid_docs: tuple[tuple[str, str], ...] = (
            ("unknown.json", '{"unknown": true}'),
            ("null-list.json", '{"reminders": null}'),
            (
                "duplicate-name.json",
                '{"reminders":[{"name":"r","emit":{"skill":"s"}},{"name":"r","emit":{"skill":"s"}}]}',
            ),
            (
                "duplicate-key.json",
                '{"reminders":[],"reminders":[]}',
            ),
            (
                "reserved.json",
                '{"reminders":[{"name":"r","emit":{"skill":"s","fields":{"<<":"x"}}}]}',
            ),
            (
                "negative.json",
                '{"reminders":[{"name":"r","cadence_secs":-1,"emit":{"skill":"s"}}]}',
            ),
            (
                "overflow.json",
                '{"reminders":[{"name":"r","cadence_secs":9223372036854775808,"emit":{"skill":"s"}}]}',
            ),
            (
                "unknown-dependency.json",
                '{"reminders":[{"name":"r","depends_on":["missing"],"emit":{"skill":"s"}}]}',
            ),
            (
                "dependency-cycle.json",
                '{"reminders":[{"name":"a","depends_on":["b"],"emit":{"skill":"s"}},'
                '{"name":"b","depends_on":["a"],"emit":{"skill":"s"}}]}',
            ),
            ("duplicate.yaml", "reminders: []\nreminders: []\n"),
            ("non-string.yaml", "1: value\nreminders: []\n"),
            ("merge.yaml", "base: &b {description: x}\n<<: *b\nreminders: []\n"),
            (
                "nonfinite.yaml",
                "reminders:\n  - {name: r, gate: .nan, emit: {skill: s}}\n",
            ),
        )
        for name, body in invalid_docs:
            path = os.path.join(tmp, name)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(body)
            _record_same_exit(rep, f"reject:{name}", py, rs, ("json", "--config", path), 2)

        for value in ("9223372036854775808", "-9223372036854775809"):
            _record_same_exit(
                rep,
                f"cli-i64:{value}",
                py,
                rs,
                ("tick", "--config", json_path, "--now", value),
                2,
            )
        _record_same_exit(
            rep,
            "cli-domain:negative-now",
            py,
            rs,
            ("tick", "--config", json_path, "--now", "-1"),
            2,
        )
        _record_same_exit(
            rep,
            "cli-domain:zero-current-cadence",
            py,
            rs,
            ("tick", "--config", json_path, "--current-tick-min", "0"),
            2,
        )
        _record_same_exit(
            rep,
            "cli:abbreviation-refused",
            py,
            rs,
            ("tick", "--conf", json_path, "--now", "0"),
            2,
        )
        for hostile_integer in ("1_0", " 10", "١٠"):
            _record_same_exit(
                rep,
                f"cli:integer-syntax:{hostile_integer!r}",
                py,
                rs,
                ("tick", "--config", json_path, f"--now={hostile_integer}"),
                2,
            )

        rng = random.Random(seed)
        for index in range(rand_count):
            reminders: list[object] = []
            for reminder_index in range(rng.randint(0, 6)):
                kind = "action" if rng.random() < 0.7 else "note"
                emit: dict[str, object] = {
                    "kind": kind,
                    "title": f"random {index}:{reminder_index} \u2603",
                }
                if kind == "action":
                    emit["skill"] = f"handler-{reminder_index}"
                    emit["fields"] = {"iteration": str(index)}
                reminders.append(
                    {
                        "name": f"r{reminder_index}",
                        "cadence_secs": rng.choice((0, 1, 30, 3600)),
                        "emit": emit,
                    }
                )
            random_config: dict[str, object] = {
                "description": f"random fixture {index}",
                "reminders": reminders,
                "health_checks": [],
            }
            path = os.path.join(tmp, f"random-{index}.json")
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(random_config, handle, ensure_ascii=False)
            _record_exact(rep, f"random:{index}/json", py, rs, ("json", "--config", path))
            _record_exact(
                rep,
                f"random:{index}/tick",
                py,
                rs,
                ("tick", "--config", path, "--now", str(index), "--no-header"),
            )

    if rep.failures:
        return divergence_verdict(tool, rep, f"{rep.checks} checks", frozenset())
    # No case here may skip; if one ever does, the default verdict refuses to call it a pass.
    return coverage_verdict(
        tool, rep, f"{rep.checks} deterministic, randomized, and adversarial checks agree", frozenset()
    )


def compare_pr_landing_planner(rand_count: int, seed: int) -> int:
    tool = "pr-landing-planner"
    py = py_command_for(tool)
    rs = rs_command(tool)
    rep = Report()

    def review_digest(
        head: str, decision: str, events: Sequence[Mapping[str, object]]
    ) -> str:
        digest = hashlib.sha256(b"pr-landing-planner-review-evidence-v3")

        def feed(value: str) -> None:
            encoded = value.encode("utf-8")
            digest.update(len(encoded).to_bytes(8, "big"))
            digest.update(encoded)

        def event_field(event: Mapping[str, object], field: str) -> str:
            value = event.get(field)
            if value is None and field in (
                "author",
                "last_edited_at",
                "retirement_actor_permission",
            ):
                return ""
            if not isinstance(value, str):
                raise ValueError(f"review event field {field!r} is not a string")
            return value

        ascii_case = re.ASCII | re.IGNORECASE
        ascii_whitespace = " \t\r\n\v\f"
        fence_re = re.compile(r"^ {0,3}(?P<f>`{3,}|~{3,})[ \t]*(?P<info>.*)$")
        block_prefix = re.compile(r"^(?:#{1,6}[ \t]+|[-+*][ \t]+)")
        disclosure = re.compile(
            r"^\[[A-Za-z0-9_.-]+,[ \t]*[A-Za-z0-9_.-]+,[ \t]*"
            r"[^,\[\]\r\n]+,[ \t]*[A-Za-z0-9_.-]+,[ \t]*"
            r"role=[A-Za-z0-9_.-]+\]\r?$",
            ascii_case,
        )
        review_marker = re.compile(
            r"^(?:CHANGES-REQUESTED-WITHDRAWN-AT|CHANGES-REQUESTED-AT|"
            r"APPROVED-AT):[ \t]*(?:claude|codex)[ \t]+[0-9a-f]{40}",
            ascii_case,
        )
        by_identity = re.compile(
            r"[ \t]+BY[ \t]+[A-Za-z0-9_.-]+$", ascii_case
        )
        who_metadata = re.compile(
            r"^Unverified --who metadata: [A-Za-z0-9_.-]+\r?$", re.ASCII
        )
        withdrawal_marker = re.compile(
            r"^CHANGES-REQUESTED-WITHDRAWN-AT:[ \t]*"
            r"(?P<lane>claude|codex)[ \t]+(?P<head>[0-9a-f]{40})",
            ascii_case,
        )
        retirement_target = re.compile(
            r"^[ \t]*RETIRES[ \t]+#?([0-9]{6,})[ \t]*\r?$", ascii_case
        )
        def ascii_strip(value: str) -> str:
            return value.strip(ascii_whitespace)

        def prose_line_indexes(body: str) -> frozenset[int]:
            indexes: set[int] = set()
            fence = ""
            indented = False
            previous_blank = True
            for index, raw in enumerate(body.split("\n")):
                blank = not ascii_strip(raw)
                if fence:
                    match = fence_re.match(raw)
                    if (
                        match is not None
                        and match.group("f")[0] == fence[0]
                        and len(match.group("f")) >= len(fence)
                        and not ascii_strip(match.group("info"))
                    ):
                        fence = ""
                    previous_blank = blank
                    continue
                match = fence_re.match(raw)
                if match is not None:
                    fence = match.group("f")
                    indented = False
                    previous_blank = False
                    continue
                if indented:
                    if blank:
                        previous_blank = True
                        continue
                    if raw.startswith(("    ", "\t")):
                        continue
                    indented = False
                elif previous_blank and raw.startswith(("    ", "\t")) and not blank:
                    indented = True
                    previous_blank = False
                    continue
                indexes.add(index)
                previous_blank = blank
            return frozenset(indexes)

        def undecorate(line: str) -> str:
            normalized = ascii_strip(block_prefix.sub("", ascii_strip(line)))
            while True:
                for wrapper in ("`", "**", "__", "*", "_"):
                    if (
                        normalized.startswith(wrapper)
                        and normalized.endswith(wrapper)
                        and len(normalized) > 2 * len(wrapper)
                    ):
                        normalized = ascii_strip(
                            normalized[len(wrapper) : -len(wrapper)]
                        )
                        break
                else:
                    return normalized

        def marker_match(pattern: re.Pattern[str], line: str) -> re.Match[str] | None:
            normalized = undecorate(line)
            match = pattern.match(normalized)
            if match is None:
                return None
            end = match.end()
            return (
                match if end == len(normalized) or normalized[end] in " \t" else None
            )

        def normalized_review_body(body: str) -> str:
            lines = body.split("\n")
            prose = prose_line_indexes(body)
            first_nonblank = next(
                (index for index, line in enumerate(lines) if ascii_strip(line)), None
            )
            out: list[str] = []
            for index, raw in enumerate(lines):
                if index == first_nonblank and index in prose and disclosure.fullmatch(raw):
                    continue
                if index in prose and who_metadata.fullmatch(raw):
                    continue
                if index in prose and marker_match(review_marker, raw) is not None:
                    marker_line = undecorate(raw)
                    claimed = by_identity.search(marker_line)
                    if claimed is not None:
                        claimed_text = claimed.group(0)
                        claimed_start = raw.rfind(claimed_text)
                        if claimed_start >= 0:
                            raw = (
                                raw[:claimed_start]
                                + raw[claimed_start + len(claimed_text) :]
                            )
                out.append(raw)
            return "\n".join(out)

        def canonical_event(event: Mapping[str, object]) -> tuple[str, ...]:
            body = event_field(event, "body")
            lines = body.split("\n")
            prose = prose_line_indexes(body)
            targets = [
                match.group(1)
                for index, line in enumerate(lines)
                if index in prose
                and (match := retirement_target.fullmatch(line)) is not None
            ]
            if targets:
                withdrawals = []
                for index, line in enumerate(lines):
                    if index not in prose:
                        continue
                    match = marker_match(withdrawal_marker, line)
                    if match is not None:
                        withdrawals.append(
                            (match.group("lane").lower(), match.group("head").lower())
                        )
                if len(targets) != 1 or len(withdrawals) != 1:
                    raise ValueError("review retirement is not canonical")
            body = normalized_review_body(body)
            permission = event_field(event, "retirement_actor_permission")
            author = event_field(event, "author") if permission else ""
            return (
                event_field(event, "kind"),
                event_field(event, "identity"),
                author,
                event_field(event, "state"),
                event_field(event, "head_sha"),
                event_field(event, "created_at"),
                event_field(event, "updated_at"),
                event_field(event, "last_edited_at"),
                body,
                permission,
            )

        feed(head)
        feed(decision)
        ordered = sorted(canonical_event(event) for event in events)
        digest.update(len(ordered).to_bytes(8, "big"))
        for event in ordered:
            for value in event:
                feed(value)
        return digest.hexdigest()

    _record_exact(rep, "version", py, rs, ("--version",))
    compare_package_guides(tool, py, rs, rep)
    command_flags = (
        "--repo",
        "--base",
        "--fixture",
        "--format",
        "--git-dir",
        "--prs",
        "--conflict-detector",
        "--gate-check",
        "--landing-context",
        "--flaky-signatures",
        "--outage-min-prs",
        "--freshness-max-behind",
        "--priority-source",
        "--batch",
    )
    for engine, command in (("py", py), ("rs", rs)):
        outcome = run(command, ("--help",))
        commands = ("plan", "graph", "clusters", "status", "quickstart")
        missing = [token for token in commands if token not in outcome.stdout]
        if outcome.returncode != 0 or missing:
            rep.bad(
                f"schema:{engine}/commands",
                f"exit={outcome.returncode}; missing={missing}\n{outcome.stdout}",
            )
        else:
            rep.ok(f"schema:{engine}/commands")
        plan_help = run(command, ("plan", "--help"))
        missing_flags = [token for token in command_flags if token not in plan_help.stdout]
        neutral_priority_help = (
            "command" in plan_help.stdout and "beads" not in plan_help.stdout.lower()
        )
        if plan_help.returncode != 0 or missing_flags or not neutral_priority_help:
            rep.bad(
                f"schema:{engine}/plan",
                f"exit={plan_help.returncode}; missing={missing_flags}; "
                f"neutral_priority_help={neutral_priority_help}\n{plan_help.stdout}",
            )
        else:
            rep.ok(f"schema:{engine}/plan")

    with tempfile.TemporaryDirectory(prefix="planner-cross-") as tmp:
        for label, payload, message in (
            ("empty", "", "empty stdout"),
            (
                "non-object-entry",
                json.dumps([{"number": 1}, "not-an-object"]),
                "entry 1 is not an object",
            ),
            (
                "limit-reached",
                json.dumps([{"number": number} for number in range(500)]),
                "500-item limit",
            ),
        ):
            fake_gh = os.path.join(tmp, f"fake-gh-list-{label}")
            Path(fake_gh).write_text(
                "#!/bin/sh\nprintf %s " + shlex.quote(payload) + "\n",
                encoding="utf-8",
            )
            os.chmod(fake_gh, 0o755)
            py_list, rs_list = _record_same_exit(
                rep,
                f"reject:live-list-{label}",
                py,
                rs,
                (
                    "plan",
                    "--repo",
                    "OWNER/NAME",
                    "--git-dir",
                    tmp,
                    "--gh-cmd",
                    fake_gh,
                    "--no-archive",
                ),
                2,
            )
            if message in py_list.stderr and message in rs_list.stderr:
                rep.ok(f"reject:live-list-{label}-is-explicit")
            else:
                rep.bad(
                    f"reject:live-list-{label}-is-explicit",
                    f"py={py_list.stderr!r}; rs={rs_list.stderr!r}",
                )

        fixture: dict[str, object] = {
            "repo": "OWNER/NAME",
            "base": "main",
            "prs": [
                {
                    "number": 1,
                    "title": "green",
                    "head_ref": "green",
                    "additions": 4,
                    "changed_files": ["src/a.rs"],
                    "checks": [
                        {"name": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
                        {"name": "merge-gate", "status": "COMPLETED", "conclusion": "SUCCESS"},
                    ],
                },
                {
                    "number": 2,
                    "title": "behind",
                    "head_ref": "behind",
                    "commits_behind": 3,
                    "changed_files": ["src/b.rs"],
                    "checks": [
                        {"name": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
                        {"name": "merge-gate", "status": "COMPLETED", "conclusion": "SUCCESS"},
                    ],
                },
                {
                    "number": 3,
                    "title": "stale gate",
                    "head_ref": "stale",
                    "changed_files": ["src/c.rs"],
                    "checks": [
                        {"name": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
                        {
                            "name": "merge-gate",
                            "status": "COMPLETED",
                            "conclusion": "FAILURE",
                            "text": "stale",
                        },
                    ],
                },
                {
                    "number": 4,
                    "title": "gate race",
                    "head_ref": "race",
                    "changed_files": ["src/d.rs"],
                    "checks": [
                        {"name": "CI", "status": "IN_PROGRESS", "conclusion": ""},
                        {
                            "name": "merge-gate",
                            "status": "COMPLETED",
                            "conclusion": "FAILURE",
                            "text": "Full CI still queued; rerun after CI completes",
                        },
                    ],
                },
                {
                    "number": 5,
                    "title": "real failure",
                    "head_ref": "red",
                    "changed_files": ["src/e.rs"],
                    "checks": [
                        {
                            "name": "unit",
                            "status": "COMPLETED",
                            "conclusion": "FAILURE",
                            "text": "assertion",
                        },
                        {"name": "merge-gate", "status": "COMPLETED", "conclusion": "SUCCESS"},
                    ],
                },
                {
                    "number": 6,
                    "title": "missing required gate",
                    "head_ref": "missing-gate",
                    "changed_files": ["src/f.rs"],
                    "checks": [
                        {"name": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"}
                    ],
                },
                {
                    "number": 7,
                    "title": "neutral required gate",
                    "head_ref": "neutral-gate",
                    "changed_files": ["src/g.rs"],
                    "checks": [
                        {"name": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
                        {"name": "merge-gate", "status": "COMPLETED", "conclusion": "NEUTRAL"},
                    ],
                },
                {
                    "number": 8,
                    "title": "non-gate queued words",
                    "head_ref": "real-queued",
                    "changed_files": ["src/h.rs"],
                    "checks": [
                        {
                            "name": "unit",
                            "status": "COMPLETED",
                            "conclusion": "FAILURE",
                            "text": "dependency still queued",
                        },
                        {"name": "merge-gate", "status": "COMPLETED", "conclusion": "SUCCESS"},
                    ],
                },
                {
                    "number": 9,
                    "title": "draft policy change",
                    "head_ref": "policy",
                    "is_draft": True,
                    "changed_files": ["src/i.rs"],
                    "checks": [
                        {"name": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
                        {"name": "merge-gate", "status": "COMPLETED", "conclusion": "SUCCESS"},
                    ],
                },
                {
                    "number": 10,
                    "title": "approval required",
                    "head_ref": "review-required",
                    "review_decision": "REVIEW_REQUIRED",
                    "changed_files": ["src/j.rs"],
                    "checks": [
                        {"name": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
                        {"name": "merge-gate", "status": "COMPLETED", "conclusion": "SUCCESS"},
                    ],
                },
                {
                    "number": 11,
                    "title": "changes requested",
                    "head_ref": "changes-requested",
                    "review_decision": "CHANGES_REQUESTED",
                    "changed_files": ["src/k.rs"],
                    "checks": [
                        {"name": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
                        {"name": "merge-gate", "status": "COMPLETED", "conclusion": "SUCCESS"},
                    ],
                },
                {
                    "number": 12,
                    "title": "green child of broken predecessor",
                    "head_ref": "child-of-red",
                    "base_ref": "red",
                    "changed_files": ["src/l.rs"],
                    "checks": [
                        {"name": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
                        {"name": "merge-gate", "status": "COMPLETED", "conclusion": "SUCCESS"},
                    ],
                },
                {
                    "number": 13,
                    "title": "cycle root",
                    "head_ref": "cycle-root",
                    "changed_files": ["src/m.rs"],
                    "checks": [
                        {"name": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
                        {"name": "merge-gate", "status": "COMPLETED", "conclusion": "SUCCESS"},
                    ],
                },
                {
                    "number": 14,
                    "title": "cycle child",
                    "head_ref": "cycle-child",
                    "base_ref": "cycle-root",
                    "changed_files": ["src/n.rs"],
                    "checks": [
                        {"name": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"},
                        {"name": "merge-gate", "status": "COMPLETED", "conclusion": "SUCCESS"},
                    ],
                },
            ],
            "conflicts": [{"a": 1, "b": 2, "paths": ["src/shared.rs"]}],
            "ancestry": [
                {"before": 1, "after": 3},
                {"before": 14, "after": 13},
            ],
        }
        fixture_path = os.path.join(tmp, "fixture.json")
        with open(fixture_path, "w", encoding="utf-8") as handle:
            json.dump(fixture, handle)
        context = {
            "prs": [
                {
                    "pr": 9,
                    "head_sha": "sha-9",
                    "base_sha": "basesha-main",
                    "validation_evidence": "clean-validate-record",
                    "validation_authority": "hard-green",
                    "policy_class": "gate-policy",
                    "assigned_agent": "reviewer",
                }
            ]
        }
        context_path = os.path.join(tmp, "context.json")
        with open(context_path, "w", encoding="utf-8") as handle:
            json.dump(context, handle)

        base_args = ("--fixture", fixture_path, "--landing-context", context_path)
        for planner_command, formats in (
            ("plan", ("human", "json", "actions")),
            ("graph", ("human", "json")),
            ("clusters", ("human", "json")),
            ("status", ("human", "json")),
        ):
            for output_format in formats:
                command_extra = ("--batch",) if planner_command == "plan" else ()
                _record_exact(
                    rep,
                    f"representative:{planner_command}/{output_format}",
                    py,
                    rs,
                    (planner_command, *base_args, "--format", output_format, *command_extra),
                )

        _record_exact(
            rep,
            "representative:command-priority",
            py,
            rs,
            (
                "plan",
                *base_args,
                "--format",
                "json",
                "--priority-source",
                "command",
                "--priority-cmd",
                "printf '%s' '{pr}'",
            ),
            compare_stderr=True,
        )

        for label, priority_args in (
            ("missing-command", ("--priority-source", "command")),
            (
                "missing-label-capture",
                ("--priority-source", "labels", "--priority-label-pattern", "^p[0-9]+$"),
            ),
            (
                "overflow-command-output",
                (
                    "--priority-source",
                    "command",
                    "--priority-cmd",
                    "printf 9223372036854775808",
                ),
            ),
            (
                "failed-command",
                ("--priority-source", "command", "--priority-cmd", "exit 9"),
            ),
            (
                "hook-launch-failure",
                (
                    "--priority-source",
                    "command",
                    "--priority-cmd",
                    "printf 1",
                    "--net-wrapper",
                    "/definitely/missing-priority-wrapper",
                ),
            ),
        ):
            _record_exact(
                rep,
                f"reject:priority/{label}",
                py,
                rs,
                ("plan", *base_args, *priority_args),
                expected=2,
                compare_stderr=True,
            )

        safety_args = ("plan", *base_args, "--format", "json", "--batch")
        default_safety, _ = _record_exact(
            rep, "representative:default-freshness", py, rs, safety_args, expected=0
        )
        # Preserve the safety table's rebase case under an explicit caller bound.
        # The production default intentionally applies no freshness reroute.
        plan, _ = _record_exact(
            rep,
            "representative:safety-actions-explicit-freshness",
            py,
            rs,
            (*safety_args, "--freshness-max-behind", "0"),
            expected=0,
        )
        ok, parsed = _parsed_json(plan.stdout)
        actions: dict[int, str] = {}
        if ok and isinstance(parsed, dict):
            plan_obj = parsed.get("plan")
            if isinstance(plan_obj, dict):
                decisions = plan_obj.get("per_pr_actions")
                if isinstance(decisions, list):
                    for decision in decisions:
                        if isinstance(decision, dict):
                            number = decision.get("pr")
                            action = decision.get("action")
                            if isinstance(number, int) and isinstance(action, str):
                                actions[number] = action
        expected_actions = {
            1: "land-now",
            2: "rebase-then-land",
            3: "refire-stale-gate",
            4: "wait",
            5: "hold-fix",
            6: "refire-ci",
            7: "refire-ci",
            8: "hold-fix",
            9: "escalate-gate-policy",
            10: "wait",
            11: "wait",
            12: "wait",
            13: "wait",
            14: "wait",
        }
        if plan.returncode == 0 and all(actions.get(number) == action for number, action in expected_actions.items()):
            rep.ok("representative:safety-actions")
        else:
            rep.bad("representative:safety-actions", f"expected={expected_actions}; got={actions}")

        default_safety_ok, default_safety_payload = _parsed_json(default_safety.stdout)
        default_safety_plan = (
            default_safety_payload.get("plan")
            if default_safety_ok and isinstance(default_safety_payload, dict)
            else None
        )
        default_pr2_actions = (
            [
                decision.get("action")
                for decision in default_safety_plan.get("per_pr_actions", [])
                if isinstance(decision, dict) and decision.get("pr") == 2
            ]
            if isinstance(default_safety_plan, dict)
            else []
        )
        default_lands_behind = default_pr2_actions == ["land-now"]
        if default_safety.returncode == 0 and default_lands_behind:
            rep.ok("representative:default-freshness-lands-behind")
        else:
            rep.bad(
                "representative:default-freshness-lands-behind",
                f"exit={default_safety.returncode}; land_now={default_lands_behind}",
            )

        if ok and isinstance(parsed, dict):
            nodes = parsed.get("nodes")
            held = parsed.get("held_prs")
            plan_obj = parsed.get("plan")
            node_schema_ok = isinstance(nodes, list) and all(
                isinstance(node, dict)
                and "base_sha" in node
                and "review_decision" in node
                for node in nodes
            )
            held_reasons: dict[int, list[str]] = {}
            if isinstance(held, list):
                for item in held:
                    if not isinstance(item, dict):
                        continue
                    number = item.get("pr")
                    reasons = item.get("reasons")
                    if (
                        isinstance(number, int)
                        and isinstance(reasons, list)
                        and all(isinstance(reason, str) for reason in reasons)
                    ):
                        held_reasons[number] = reasons
            batch_values = plan_obj.get("batch") if isinstance(plan_obj, dict) else None
            safety_schema_ok = (
                node_schema_ok
                and "review-required" in held_reasons.get(10, [])
                and "changes-requested" in held_reasons.get(11, [])
                and "ordering-cycle" in held_reasons.get(13, [])
                and "ordering-cycle" in held_reasons.get(14, [])
                and isinstance(batch_values, list)
                and not any(number in batch_values for number in (12, 13, 14))
            )
            if safety_schema_ok:
                rep.ok("representative:safety-schema")
            else:
                rep.bad(
                    "representative:safety-schema",
                    f"node_schema={node_schema_ok}; held={held_reasons}; batch={batch_values}",
                )
        else:
            rep.bad("representative:safety-schema", "representative plan was not JSON")

        _record_same_exit(rep, "live:repo-required", py, rs, ("plan",), 2)
        _record_same_exit(
            rep,
            "cli:abbreviation-refused",
            py,
            rs,
            ("plan", "--fixt", fixture_path),
            2,
        )
        _record_same_exit(
            rep,
            "fixture:unknown-detector",
            py,
            rs,
            ("plan", "--fixture", fixture_path, "--conflict-detector", "unknown"),
            2,
        )
        invalid_fixtures: tuple[tuple[str, str], ...] = (
            ("root-array.json", "[]"),
            ("missing-prs.json", '{"repo":"R","base":"main"}'),
            ("prs-not-list.json", '{"repo":"R","base":"main","prs":{}}'),
            ("number-not-int.json", '{"repo":"R","base":"main","prs":[{"number":"one"}]}'),
            ("malformed.json", "{"),
            ("malformed.yaml", "prs:\n  - number: [\n"),
            ("duplicate-key.json", '{"prs":[],"prs":[]}'),
            ("duplicate-key.yaml", "prs: []\nprs: []\n"),
            ("merge-key.yaml", "base: &b {repo: R}\n<<: *b\nprs: []\n"),
            ("duplicate-pr.json", '{"prs":[{"number":1},{"number":1}]}'),
            ("zero-pr.json", '{"prs":[{"number":0}]}'),
            ("overflow-pr.json", '{"prs":[{"number":9223372036854775808}]}'),
            ("negative-behind.json", '{"prs":[{"number":1,"commits_behind":-1}]}'),
            (
                "unknown-conflict.json",
                '{"prs":[{"number":1}],"conflicts":[{"a":1,"b":2}]}',
            ),
            (
                "self-ancestry.json",
                '{"prs":[{"number":1}],"ancestry":[{"before":1,"after":1}]}',
            ),
            ("relations-shape.json", '{"prs":[{"number":1}],"conflicts":{}}'),
        )
        for name, body in invalid_fixtures:
            path = os.path.join(tmp, name)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(body)
            _record_same_exit(rep, f"reject:{name}", py, rs, ("plan", "--fixture", path), 2)

        bad_contexts = (
            (
                "head-only-context.json",
                '{"prs":[{"pr":1,"head_sha":"sha-1","validation_evidence":"clean-validate-record"}]}',
            ),
            (
                "exact-base-no-authority-context.json",
                '{"prs":[{"pr":1,"head_sha":"sha-1","base_sha":"basesha-main","validation_evidence":"clean-validate-record"}]}',
            ),
            (
                "stale-base-context.json",
                '{"prs":[{"pr":1,"head_sha":"sha-1","base_sha":"old-base","validation_evidence":"clean-validate-record"}]}',
            ),
            (
                "hard-green-other-base-context.json",
                '{"prs":[{"pr":1,"head_sha":"sha-1","base_sha":"divergent-base","validation_evidence":"clean-validate-record","validation_authority":"hard-green"}]}',
            ),
            (
                "authority-without-record-context.json",
                '{"prs":[{"pr":1,"head_sha":"sha-1","base_sha":"old-base","validation_authority":"soft-green"}]}',
            ),
        )
        for name, body in bad_contexts:
            path = os.path.join(tmp, name)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(body)
            _record_same_exit(
                rep,
                f"reject:{name}",
                py,
                rs,
                ("plan", "--fixture", fixture_path, "--landing-context", path),
                2,
            )

        soft_green_path = os.path.join(tmp, "soft-green-context.json")
        with open(soft_green_path, "w", encoding="utf-8") as handle:
            handle.write(
                '{"prs":[{"pr":2,"head_sha":"sha-2","base_sha":"earlier-green-base",'
                '"validation_evidence":"clean-validate-record",'
                '"validation_authority":"soft-green"}]}'
            )
        soft_green_args = (
            "plan",
            "--fixture",
            fixture_path,
            "--landing-context",
            soft_green_path,
            "--format",
            "json",
        )
        default_soft_green, _ = _record_exact(
            rep, "accept:soft-green-earlier-base", py, rs, soft_green_args, expected=0
        )
        default_soft_ok, default_soft_payload = _parsed_json(default_soft_green.stdout)
        default_soft_nodes = (
            default_soft_payload.get("nodes")
            if default_soft_ok and isinstance(default_soft_payload, dict)
            else None
        )
        default_soft_plan = (
            default_soft_payload.get("plan")
            if default_soft_ok and isinstance(default_soft_payload, dict)
            else None
        )
        default_soft_authority = isinstance(default_soft_nodes, list) and any(
            isinstance(node, dict)
            and node.get("pr") == 2
            and node.get("validation_authority") == "soft-green"
            for node in default_soft_nodes
        )
        default_soft_pr2_actions = (
            [
                decision.get("action")
                for decision in default_soft_plan.get("per_pr_actions", [])
                if isinstance(decision, dict) and decision.get("pr") == 2
            ]
            if isinstance(default_soft_plan, dict)
            else []
        )
        default_soft_lands = default_soft_pr2_actions == ["land-now"]
        if default_soft_green.returncode == 0 and default_soft_authority and default_soft_lands:
            rep.ok("accept:soft-green-default-authority-lands-without-rebase")
        else:
            rep.bad(
                "accept:soft-green-default-authority-lands-without-rebase",
                f"exit={default_soft_green.returncode}; authority={default_soft_authority}; "
                f"land_now={default_soft_lands}",
            )

        # Keep the original authority/rebase/no-revalidation oracle, now naming
        # the caller policy that requests a rebase instead of changing the default.
        soft_green, _ = _record_exact(
            rep,
            "accept:soft-green-explicit-freshness",
            py,
            rs,
            (*soft_green_args, "--freshness-max-behind", "0"),
            expected=0,
        )
        soft_green_ok, soft_green_payload = _parsed_json(soft_green.stdout)
        soft_green_nodes = (
            soft_green_payload.get("nodes")
            if soft_green_ok and isinstance(soft_green_payload, dict)
            else None
        )
        soft_green_plan = (
            soft_green_payload.get("plan")
            if soft_green_ok and isinstance(soft_green_payload, dict)
            else None
        )
        authority_recorded = isinstance(soft_green_nodes, list) and any(
            isinstance(node, dict)
            and node.get("pr") == 2
            and node.get("validation_authority") == "soft-green"
            for node in soft_green_nodes
        )
        soft_green_rebases_without_revalidation = (
            isinstance(soft_green_plan, dict)
            and any(
                isinstance(decision, dict)
                and decision.get("pr") == 2
                and decision.get("action") == "rebase-then-land"
                and "without pre-landing revalidation" in decision.get("why", "")
                for decision in soft_green_plan.get("per_pr_actions", [])
            )
        )
        if (
            soft_green.returncode == 0
            and authority_recorded
            and soft_green_rebases_without_revalidation
        ):
            rep.ok("accept:soft-green-authority-requires-rebase-without-revalidation")
        else:
            rep.bad(
                "accept:soft-green-authority-requires-rebase-without-revalidation",
                f"exit={soft_green.returncode}; authority={authority_recorded}; "
                f"rebase_without_revalidation={soft_green_rebases_without_revalidation}",
            )

        hard_green_path = os.path.join(tmp, "hard-green-context.json")
        with open(hard_green_path, "w", encoding="utf-8") as handle:
            handle.write(
                '{"prs":[{"pr":1,"head_sha":"sha-1","base_sha":"basesha-main",'
                '"validation_evidence":"clean-validate-record",'
                '"validation_authority":"hard-green"}]}'
            )
        hard_green_args = (
            "plan",
            "--fixture",
            fixture_path,
            "--landing-context",
            hard_green_path,
            "--format",
            "json",
        )
        _record_exact(rep, "accept:hard-green-current-base", py, rs, hard_green_args)
        hard_green = run(py, hard_green_args)
        hard_green_ok, hard_green_payload = _parsed_json(hard_green.stdout)
        hard_green_nodes = (
            hard_green_payload.get("nodes")
            if hard_green_ok and isinstance(hard_green_payload, dict)
            else None
        )
        hard_green_plan = (
            hard_green_payload.get("plan")
            if hard_green_ok and isinstance(hard_green_payload, dict)
            else None
        )
        hard_authority_recorded = isinstance(hard_green_nodes, list) and any(
            isinstance(node, dict)
            and node.get("pr") == 1
            and node.get("validation_authority") == "hard-green"
            for node in hard_green_nodes
        )
        hard_green_landable = isinstance(hard_green_plan, dict) and 1 in hard_green_plan.get(
            "land_now", []
        )
        if hard_green.returncode == 0 and hard_authority_recorded and hard_green_landable:
            rep.ok("accept:hard-green-authority-is-durable-and-landable")
        else:
            rep.bad(
                "accept:hard-green-authority-is-durable-and-landable",
                f"exit={hard_green.returncode}; authority={hard_authority_recorded}; "
                f"landable={hard_green_landable}",
            )

        review_head = "a" * 40
        observed_at = "2026-09-04T12:00:00Z"
        review_events: list[dict[str, object]] = [
            {
                "kind": "review",
                "identity": "review-1",
                "author": "reviewer",
                "state": "CHANGES_REQUESTED",
                "head_sha": review_head,
                "created_at": observed_at,
                "updated_at": observed_at,
                "last_edited_at": "",
                "body": "please address this",
                "retirement_actor_permission": "",
            },
            {
                "kind": "issue-comment",
                "identity": "comment-1",
                "author": "release-authority",
                "state": "ACTIVE",
                "head_sha": "",
                "created_at": observed_at,
                "updated_at": observed_at,
                "last_edited_at": "",
                "body": (
                    "[team, release-authority, session, model, role=observer]\n"
                    f"CHANGES-REQUESTED-WITHDRAWN-AT: codex {review_head} "
                    "BY release-authority\n"
                    "RETIRES 123456"
                ),
                "retirement_actor_permission": "write",
            },
            {
                "kind": "review-comment",
                "identity": "thread-1",
                "author": "reviewer",
                "state": "RESOLVED",
                "head_sha": "",
                "created_at": observed_at,
                "updated_at": observed_at,
                "last_edited_at": "",
                "body": "inline objection retired",
                "retirement_actor_permission": "",
            },
        ]
        review_pr: dict[str, object] = {
            "number": 394,
            "head_sha": review_head,
            "review_decision": "CHANGES_REQUESTED",
            "updated_at": observed_at,
            "review_events": review_events,
            "checks": [
                {
                    "name": "merge-gate",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                }
            ],
        }
        review_fixture: dict[str, object] = {
            "repo": "R",
            "base": "main",
            "prs": [review_pr],
        }
        review_fixture_path = os.path.join(tmp, "review-evidence-fixture.json")
        with open(review_fixture_path, "w", encoding="utf-8") as handle:
            json.dump(review_fixture, handle)
        review_probe_args = (
            "plan",
            "--fixture",
            review_fixture_path,
            "--format",
            "json",
        )
        probe_py, _probe_rs = _record_exact(
            rep,
            "accept:emit-review-event-digest-before-authority",
            py,
            rs,
            review_probe_args,
            expected=0,
        )
        probe_ok, probe_payload = _parsed_json(probe_py.stdout)
        emitted_review_digest = ""
        if probe_ok and isinstance(probe_payload, dict):
            nodes = probe_payload.get("nodes")
            if isinstance(nodes, list) and len(nodes) == 1 and isinstance(nodes[0], dict):
                value = nodes[0].get("review_evidence_digest")
                if isinstance(value, str):
                    emitted_review_digest = value
        locally_computed_digest = review_digest(
            review_head, "CHANGES_REQUESTED", review_events
        )
        if emitted_review_digest == locally_computed_digest:
            rep.ok("accept:emitted-review-digest-matches-canonical-contract")
        else:
            rep.bad(
                "accept:emitted-review-digest-matches-canonical-contract",
                f"emitted={emitted_review_digest!r}; expected={locally_computed_digest!r}",
            )
        review_context_path = os.path.join(tmp, "review-evidence-context.json")
        with open(review_context_path, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "prs": [
                        {
                            "pr": 394,
                            "head_sha": review_head,
                            "review_objections_resolved": True,
                            "review_evidence_digest": emitted_review_digest,
                        }
                    ]
                },
                handle,
            )
        review_args = (
            "plan",
            "--fixture",
            review_fixture_path,
            "--landing-context",
            review_context_path,
            "--format",
            "json",
        )

        def review_disposition(outcome: Outcome, pr: int) -> tuple[bool | None, str]:
            parsed_ok, payload = _parsed_json(outcome.stdout)
            if not parsed_ok or not isinstance(payload, dict):
                return None, ""
            held: bool | None = None
            nodes = payload.get("nodes")
            if isinstance(nodes, list):
                for node in nodes:
                    if not isinstance(node, dict) or node.get("pr") != pr:
                        continue
                    held_value = node.get("held")
                    if isinstance(held_value, bool):
                        held = held_value
            action = ""
            plan = payload.get("plan")
            decisions = plan.get("per_pr_actions") if isinstance(plan, dict) else None
            if isinstance(decisions, list):
                for decision in decisions:
                    if not isinstance(decision, dict) or decision.get("pr") != pr:
                        continue
                    action_value = decision.get("action")
                    if isinstance(action_value, str):
                        action = action_value
            return held, action
        unavailable_fixture = {
            "repo": "OWNER/NAME",
            "base": "main",
            "prs": [
                {
                    "number": 1,
                    "title": "review evidence unavailable",
                    "head_ref": "green",
                    "head_sha": "sha-1",
                    "review_decision": "APPROVED",
                    "review_evidence_unavailable": True,
                    "changed_files": ["src/a.rs"],
                    "checks": [
                        {
                            "name": "merge-gate",
                            "status": "COMPLETED",
                            "conclusion": "SUCCESS",
                        }
                    ],
                }
            ],
        }
        unavailable_path = os.path.join(tmp, "review-evidence-unavailable.json")
        with open(unavailable_path, "w", encoding="utf-8") as handle:
            json.dump(unavailable_fixture, handle)
        unavailable_outcome, _unavailable_rs = _record_exact(
            rep,
            "reject:review-evidence-unavailable-with-clean-validation",
            py,
            rs,
            (
                "plan",
                "--fixture",
                unavailable_path,
                "--landing-context",
                hard_green_path,
                "--format",
                "json",
            ),
            expected=0,
        )
        unavailable_held, unavailable_action = review_disposition(
            unavailable_outcome, 1
        )
        unavailable_ok, unavailable_payload = _parsed_json(
            unavailable_outcome.stdout
        )
        unavailable_nodes = (
            unavailable_payload.get("nodes")
            if unavailable_ok and isinstance(unavailable_payload, dict)
            else None
        )
        unavailable_visible = (
            isinstance(unavailable_nodes, list)
            and len(unavailable_nodes) == 1
            and isinstance(unavailable_nodes[0], dict)
            and unavailable_nodes[0].get("review_evidence_unavailable") is True
        )
        if (
            unavailable_held is True
            and unavailable_action == "wait"
            and unavailable_visible
        ):
            rep.ok("reject:review-evidence-unavailable-remains-visible-and-held")
        else:
            rep.bad(
                "reject:review-evidence-unavailable-remains-visible-and-held",
                f"held={unavailable_held}; action={unavailable_action!r}; "
                f"visible={unavailable_visible}",
            )

        for label, conflict_field, conflict_value in (
            ("local", "base_conflict_paths", ["src/conflict.rs"]),
            ("github", "mergeable", "CONFLICTING"),
        ):
            conflict_pr: dict[str, object] = {
                "number": 1,
                "title": "review evidence unavailable with base conflict",
                "head_ref": "green",
                "head_sha": "sha-1",
                "review_decision": "APPROVED",
                "review_evidence_unavailable": True,
                "changed_files": ["src/a.rs"],
                "checks": [
                    {
                        "name": "merge-gate",
                        "status": "COMPLETED",
                        "conclusion": "SUCCESS",
                    }
                ],
            }
            conflict_pr[conflict_field] = conflict_value
            conflict_fixture: dict[str, object] = {
                "repo": "OWNER/NAME",
                "base": "main",
                "prs": [conflict_pr],
            }
            conflict_path = os.path.join(tmp, f"review-unavailable-{label}-conflict.json")
            with open(conflict_path, "w", encoding="utf-8") as handle:
                json.dump(conflict_fixture, handle)
            conflict_outcome, _conflict_rs = _record_exact(
                rep,
                f"reject:review-evidence-unavailable-before-{label}-base-conflict",
                py,
                rs,
                (
                    "plan",
                    "--fixture",
                    conflict_path,
                    "--landing-context",
                    hard_green_path,
                    "--format",
                    "json",
                ),
                expected=0,
            )
            conflict_held, conflict_action = review_disposition(conflict_outcome, 1)
            if conflict_held is True and conflict_action == "wait":
                rep.ok(f"reject:review-evidence-unavailable-{label}-conflict-waits")
            else:
                rep.bad(
                    f"reject:review-evidence-unavailable-{label}-conflict-waits",
                    f"held={conflict_held}; action={conflict_action!r}",
                )

        review_accept, _review_accept_rs = _record_exact(
            rep,
            "accept:exact-review-event-digest",
            py,
            rs,
            review_args,
            expected=0,
        )
        review_accept_held, review_accept_action = review_disposition(review_accept, 394)
        if (
            review_accept.returncode == 0
            and review_accept_held is False
            and review_accept_action == "land-now"
        ):
            rep.ok("accept:resolved-review-is-unheld-and-land-now")
        else:
            rep.bad(
                "accept:resolved-review-is-unheld-and-land-now",
                f"exit={review_accept.returncode}; held={review_accept_held}; "
                f"action={review_accept_action!r}",
            )

        no_retirement_args = (
            "plan",
            "--fixture",
            review_fixture_path,
            "--format",
            "json",
        )
        no_retirement, _no_retirement_rs = _record_exact(
            rep,
            "reject:no-retirement-authority",
            py,
            rs,
            no_retirement_args,
            expected=0,
        )
        no_retirement_held, no_retirement_action = review_disposition(no_retirement, 394)
        if (
            no_retirement.returncode == 0
            and no_retirement_held is True
            and no_retirement_action == "wait"
        ):
            rep.ok("reject:no-retirement-remains-held-and-not-landable")
        else:
            rep.bad(
                "reject:no-retirement-remains-held-and-not-landable",
                f"exit={no_retirement.returncode}; held={no_retirement_held}; "
                f"action={no_retirement_action!r}",
            )

        nullable_last_edit = dict(review_fixture)
        nullable_last_edit_pr = dict(review_pr)
        nullable_last_edit_events = [dict(event) for event in review_events]
        nullable_last_edit_events[0]["last_edited_at"] = None
        nullable_last_edit_pr["review_events"] = nullable_last_edit_events
        nullable_last_edit["prs"] = [nullable_last_edit_pr]
        nullable_last_edit_path = os.path.join(tmp, "nullable-review-last-edit.json")
        with open(nullable_last_edit_path, "w", encoding="utf-8") as handle:
            json.dump(nullable_last_edit, handle)
        _record_exact(
            rep,
            "accept:null-native-last-edit-normalizes-to-empty",
            py,
            rs,
            (
                "plan",
                "--fixture",
                nullable_last_edit_path,
                "--landing-context",
                review_context_path,
                "--format",
                "json",
            ),
        )

        same_second = dict(review_fixture)
        same_second_pr = dict(review_pr)
        same_second_pr["review_events"] = [
            *review_events,
            {
                "kind": "review-comment",
                "identity": "thread-2",
                "author": "reviewer",
                "state": "ACTIVE",
                "head_sha": "",
                "created_at": observed_at,
                "updated_at": observed_at,
                "last_edited_at": "",
                "body": "new objection in the same timestamp tick",
                "retirement_actor_permission": "",
            },
        ]
        same_second["prs"] = [same_second_pr]
        same_second_path = os.path.join(tmp, "same-timestamp-objection.json")
        with open(same_second_path, "w", encoding="utf-8") as handle:
            json.dump(same_second, handle)
        _record_same_exit(
            rep,
            "reject:same-timestamp-review-objection",
            py,
            rs,
            (
                "plan",
                "--fixture",
                same_second_path,
                "--landing-context",
                review_context_path,
            ),
            2,
        )

        for label, field, value in (
            ("body-change", "body", "changed objection text"),
            ("state-change", "state", "ACTIVE"),
            ("event-head-change", "head_sha", "b" * 40),
            ("creation-time-change", "created_at", "2026-09-04T11:59:59Z"),
            ("version-time-change", "updated_at", "2026-09-04T12:00:01Z"),
            ("edit-time-change", "last_edited_at", "2026-09-04T12:00:01Z"),
        ):
            changed_evidence = dict(review_fixture)
            changed_pr = dict(review_pr)
            changed_events = [dict(event) for event in review_events]
            changed_events[2][field] = value
            changed_pr["review_events"] = changed_events
            changed_evidence["prs"] = [changed_pr]
            changed_path = os.path.join(tmp, f"same-timestamp-review-{label}.json")
            with open(changed_path, "w", encoding="utf-8") as handle:
                json.dump(changed_evidence, handle)
            _record_same_exit(
                rep,
                f"reject:same-timestamp-review-{label}",
                py,
                rs,
                (
                    "plan",
                    "--fixture",
                    changed_path,
                    "--landing-context",
                    review_context_path,
                ),
                2,
            )

        for label, event_index, field, value in (
            ("ordinary-author-change", 2, "author", "different-reviewer"),
            (
                "false-claimed-by-hyphen",
                1,
                "body",
                str(review_events[1]["body"]).replace(
                    "BY release-authority", "BY another-agent"
                ),
            ),
            (
                "false-claimed-by-underscore",
                1,
                "body",
                str(review_events[1]["body"]).replace(
                    "BY release-authority", "BY another_agent"
                ),
            ),
            (
                "false-claimed-by-dot",
                1,
                "body",
                str(review_events[1]["body"]).replace(
                    "BY release-authority", "BY another.agent"
                ),
            ),
            (
                "false-claimed-by-ascii-k",
                1,
                "body",
                str(review_events[1]["body"]).replace(
                    "BY release-authority", "BY K"
                ),
            ),
            (
                "changed-disclosure-underscore",
                1,
                "body",
                str(review_events[1]["body"]).replace(
                    "[team, release-authority, session, model, role=observer]",
                    "[team, departed_reviewer, old-session, model, role=observer]",
                ),
            ),
            (
                "changed-disclosure-dot",
                1,
                "body",
                str(review_events[1]["body"]).replace(
                    "[team, release-authority, session, model, role=observer]",
                    "[team, departed.reviewer, old-session, model, role=observer]",
                ),
            ),
            (
                "changed-disclosure-ascii-k",
                1,
                "body",
                str(review_events[1]["body"]).replace(
                    "[team, release-authority, session, model, role=observer]",
                    "[team, K, old-session, model, role=observer]",
                ),
            ),
        ):
            metadata_fixture = dict(review_fixture)
            metadata_pr = dict(review_pr)
            metadata_events = [dict(event) for event in review_events]
            metadata_events[event_index][field] = value
            metadata_pr["review_events"] = metadata_events
            metadata_fixture["prs"] = [metadata_pr]
            metadata_path = os.path.join(tmp, f"review-metadata-{label}.json")
            with open(metadata_path, "w", encoding="utf-8") as handle:
                json.dump(metadata_fixture, handle)
            metadata_outcome, _metadata_rs = _record_exact(
                rep,
                f"accept:review-metadata-{label}",
                py,
                rs,
                (
                    "plan",
                    "--fixture",
                    metadata_path,
                    "--landing-context",
                    review_context_path,
                    "--format",
                    "json",
                ),
                expected=0,
            )
            metadata_held, metadata_action = review_disposition(metadata_outcome, 394)
            if metadata_held is False and metadata_action == "land-now":
                rep.ok(f"accept:review-metadata-{label}-decision-unchanged")
            else:
                rep.bad(
                    f"accept:review-metadata-{label}-decision-unchanged",
                    f"held={metadata_held}; action={metadata_action!r}",
                )

        base_review_digest = review_digest(
            review_head, "CHANGES_REQUESTED", review_events
        )

        def write_review_variant(
            label: str, events: list[dict[str, object]]
        ) -> str:
            variant = dict(review_fixture)
            variant_pr = dict(review_pr)
            variant_pr["review_events"] = events
            variant["prs"] = [variant_pr]
            path = os.path.join(tmp, f"review-{label}.json")
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(variant, handle)
            return path

        def write_review_context(label: str, digest: str) -> str:
            path = os.path.join(tmp, f"review-{label}-context.json")
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(
                    {
                        "prs": [
                            {
                                "pr": 394,
                                "head_sha": review_head,
                                "review_objections_resolved": True,
                                "review_evidence_digest": digest,
                            }
                        ]
                    },
                    handle,
                )
            return path

        ascii_k_events = [dict(event) for event in review_events]
        ascii_k_events[1]["body"] = str(ascii_k_events[1]["body"]).replace(
            "BY release-authority", "BY K"
        )
        ascii_k_digest = review_digest(
            review_head, "CHANGES_REQUESTED", ascii_k_events
        )
        if ascii_k_digest == base_review_digest:
            rep.ok("accept:ascii-k-by-is-canonical-metadata")
        else:
            rep.bad(
                "accept:ascii-k-by-is-canonical-metadata",
                f"base={base_review_digest}; ascii_k={ascii_k_digest}",
            )

        for label, non_ascii_identity in (
            ("kelvin-sign", "K"),
            ("capital-dotted-i", "İ"),
            ("dotless-i", "ı"),
            ("long-s", "ſ"),
        ):
            non_ascii_by_events = [dict(event) for event in review_events]
            non_ascii_by_events[1]["body"] = str(
                non_ascii_by_events[1]["body"]
            ).replace("BY release-authority", f"BY {non_ascii_identity}")
            non_ascii_by_digest = review_digest(
                review_head, "CHANGES_REQUESTED", non_ascii_by_events
            )
            if non_ascii_by_digest != ascii_k_digest:
                rep.ok(f"reject:non-ascii-by-{label}-remains-in-digest")
            else:
                rep.bad(
                    f"reject:non-ascii-by-{label}-remains-in-digest",
                    "non-ASCII identity was normalized as ASCII metadata",
                )
            non_ascii_by_path = write_review_variant(
                f"non-ascii-by-{label}", non_ascii_by_events
            )
            _record_same_exit(
                rep,
                f"reject:non-ascii-by-{label}-invalidates-context",
                py,
                rs,
                (
                    "plan",
                    "--fixture",
                    non_ascii_by_path,
                    "--landing-context",
                    review_context_path,
                ),
                2,
            )

            non_ascii_disclosure_events = [dict(event) for event in review_events]
            non_ascii_disclosure_events[1]["body"] = str(
                non_ascii_disclosure_events[1]["body"]
            ).replace(
                "[team, release-authority, session, model, role=observer]",
                f"[team, {non_ascii_identity}, session, model, role=observer]",
            )
            non_ascii_disclosure_digest = review_digest(
                review_head, "CHANGES_REQUESTED", non_ascii_disclosure_events
            )
            if non_ascii_disclosure_digest != base_review_digest:
                rep.ok(f"reject:non-ascii-disclosure-{label}-remains-in-digest")
            else:
                rep.bad(
                    f"reject:non-ascii-disclosure-{label}-remains-in-digest",
                    "non-ASCII disclosure was normalized as identity metadata",
                )
            non_ascii_disclosure_path = write_review_variant(
                f"non-ascii-disclosure-{label}", non_ascii_disclosure_events
            )
            _record_same_exit(
                rep,
                f"reject:non-ascii-disclosure-{label}-invalidates-context",
                py,
                rs,
                (
                    "plan",
                    "--fixture",
                    non_ascii_disclosure_path,
                    "--landing-context",
                    review_context_path,
                ),
                2,
            )

            non_ascii_wrapper_events = [dict(event) for event in review_events]
            for event in non_ascii_wrapper_events:
                event["body"] = (
                    f"{event['body']}\n"
                    f"Unverified --who metadata: {non_ascii_identity}"
                )
            non_ascii_wrapper_digest = review_digest(
                review_head, "CHANGES_REQUESTED", non_ascii_wrapper_events
            )
            if non_ascii_wrapper_digest != base_review_digest:
                rep.ok(f"reject:non-ascii-who-metadata-{label}-remains-in-digest")
            else:
                rep.bad(
                    f"reject:non-ascii-who-metadata-{label}-remains-in-digest",
                    "non-ASCII who metadata was normalized",
                )
            non_ascii_wrapper_path = write_review_variant(
                f"non-ascii-who-metadata-{label}", non_ascii_wrapper_events
            )
            _record_same_exit(
                rep,
                f"reject:non-ascii-who-metadata-{label}-invalidates-context",
                py,
                rs,
                (
                    "plan",
                    "--fixture",
                    non_ascii_wrapper_path,
                    "--landing-context",
                    review_context_path,
                ),
                2,
            )
        for whitespace_label, non_ascii_space in (
            ("nbsp", "\u00a0"),
            ("em-space", "\u2003"),
        ):
            source_body = str(review_events[1]["body"])
            whitespace_variants = (
                (
                    "by",
                    source_body.replace(
                        "BY release-authority",
                        f"BY{non_ascii_space}release-authority",
                    ),
                ),
                (
                    "disclosure",
                    source_body.replace(
                        "[team, release-authority, session, model, role=observer]",
                        "[team,"
                        f"{non_ascii_space}release-authority, session, model, role=observer]",
                    ),
                ),
                (
                    "who-metadata",
                    source_body
                    + f"\nUnverified --who metadata:{non_ascii_space}K",
                ),
            )
            for grammar, changed_body in whitespace_variants:
                changed_events = [dict(event) for event in review_events]
                changed_events[1]["body"] = changed_body
                changed_digest = review_digest(
                    review_head, "CHANGES_REQUESTED", changed_events
                )
                if changed_digest != base_review_digest:
                    rep.ok(
                        f"reject:non-ascii-whitespace-{whitespace_label}-{grammar}-digest"
                    )
                else:
                    rep.bad(
                        f"reject:non-ascii-whitespace-{whitespace_label}-{grammar}-digest",
                        "non-ASCII whitespace was normalized as review metadata",
                    )
                changed_path = write_review_variant(
                    f"non-ascii-whitespace-{whitespace_label}-{grammar}",
                    changed_events,
                )
                _record_same_exit(
                    rep,
                    f"reject:non-ascii-whitespace-{whitespace_label}-{grammar}-context",
                    py,
                    rs,
                    (
                        "plan",
                        "--fixture",
                        changed_path,
                        "--landing-context",
                        review_context_path,
                    ),
                    2,
                )

            for grammar, changed_body in (
                (
                    "withdrawal",
                    source_body.replace(
                        "WITHDRAWN-AT: codex",
                        f"WITHDRAWN-AT:{non_ascii_space}codex",
                    ),
                ),
                (
                    "retirement",
                    source_body.replace("RETIRES 123456", f"RETIRES{non_ascii_space}123456"),
                ),
            ):
                malformed_events = [dict(event) for event in review_events]
                malformed_events[1]["body"] = changed_body
                malformed_path = write_review_variant(
                    f"non-ascii-whitespace-{whitespace_label}-{grammar}",
                    malformed_events,
                )
                _record_same_exit(
                    rep,
                    f"reject:non-ascii-whitespace-{whitespace_label}-{grammar}",
                    py,
                    rs,
                    ("plan", "--fixture", malformed_path),
                    2,
                )

        malformed_by_digests: list[str] = []
        for label, malformed_by in (
            ("slash", "BY release/authority"),
            ("space", "BY release authority"),
            ("colon", "BY: release.authority"),
            ("compact", "BY/release.authority"),
        ):
            malformed_events = [dict(event) for event in review_events]
            malformed_events[1]["body"] = str(malformed_events[1]["body"]).replace(
                "BY release-authority", malformed_by
            )
            malformed_digest = review_digest(
                review_head, "CHANGES_REQUESTED", malformed_events
            )
            malformed_by_digests.append(malformed_digest)
            malformed_path = write_review_variant(
                f"malformed-by-{label}", malformed_events
            )
            _record_same_exit(
                rep,
                f"reject:review-malformed-by-{label}-preserves-bytes",
                py,
                rs,
                (
                    "plan",
                    "--fixture",
                    malformed_path,
                    "--landing-context",
                    review_context_path,
                ),
                2,
            )
            malformed_context_path = write_review_context(
                f"malformed-by-{label}", malformed_digest
            )
            malformed_outcome, _malformed_rs = _record_exact(
                rep,
                f"accept:review-malformed-by-{label}-keeps-withdrawal",
                py,
                rs,
                (
                    "plan",
                    "--fixture",
                    malformed_path,
                    "--landing-context",
                    malformed_context_path,
                    "--format",
                    "json",
                ),
                expected=0,
            )
            malformed_held, malformed_action = review_disposition(
                malformed_outcome, 394
            )
            if malformed_held is False and malformed_action == "land-now":
                rep.ok(f"accept:review-malformed-by-{label}-decision-unchanged")
            else:
                rep.bad(
                    f"accept:review-malformed-by-{label}-decision-unchanged",
                    f"held={malformed_held}; action={malformed_action!r}",
                )
        if (
            all(value != base_review_digest for value in malformed_by_digests)
            and len(set(malformed_by_digests)) == len(malformed_by_digests)
        ):
            rep.ok("reject:malformed-by-mutations-change-review-digest")
        else:
            rep.bad(
                "reject:malformed-by-mutations-change-review-digest",
                f"base={base_review_digest}; malformed={malformed_by_digests}",
            )

        for agent in ("review-agent", "review_agent", "review.agent", "K"):
            wrapper_events = [dict(event) for event in review_events]
            for event in wrapper_events:
                event["body"] = (
                    f"{event['body']}\nUnverified --who metadata: {agent}"
                )
            wrapper_digest = review_digest(
                review_head, "CHANGES_REQUESTED", wrapper_events
            )
            if wrapper_digest == base_review_digest:
                rep.ok(f"accept:who-metadata-{agent}-digest-unchanged")
            else:
                rep.bad(
                    f"accept:who-metadata-{agent}-digest-unchanged",
                    f"base={base_review_digest}; wrapper={wrapper_digest}",
                )
            wrapper_path = write_review_variant(
                f"who-metadata-{agent}", wrapper_events
            )
            wrapper_outcome, _wrapper_rs = _record_exact(
                rep,
                f"accept:who-metadata-{agent}",
                py,
                rs,
                (
                    "plan",
                    "--fixture",
                    wrapper_path,
                    "--landing-context",
                    review_context_path,
                    "--format",
                    "json",
                ),
                expected=0,
            )
            wrapper_held, wrapper_action = review_disposition(wrapper_outcome, 394)
            if wrapper_held is False and wrapper_action == "land-now":
                rep.ok(f"accept:who-metadata-{agent}-decision-unchanged")
            else:
                rep.bad(
                    f"accept:who-metadata-{agent}-decision-unchanged",
                    f"held={wrapper_held}; action={wrapper_action!r}",
                )

        preserved_wrapper_events = [dict(event) for event in review_events]
        preserved_wrapper_events[0]["body"] = (
            f"{preserved_wrapper_events[0]['body']}\n"
            "> Unverified --who metadata: review.agent"
        )
        preserved_wrapper_events[1]["body"] = (
            f"{preserved_wrapper_events[1]['body']}\n"
            "Unverified --who metadata: review.agent extra"
        )
        preserved_wrapper_events[2]["body"] = (
            f"{preserved_wrapper_events[2]['body']}\n```text\n"
            "Unverified --who metadata: review.agent\n```"
        )
        preserved_wrapper_digest = review_digest(
            review_head, "CHANGES_REQUESTED", preserved_wrapper_events
        )
        changed_preserved_events = [dict(event) for event in preserved_wrapper_events]
        for event in changed_preserved_events:
            event["body"] = str(event["body"]).replace(
                "review.agent", "other_agent"
            )
        changed_preserved_digest = review_digest(
            review_head, "CHANGES_REQUESTED", changed_preserved_events
        )
        if (
            preserved_wrapper_digest != base_review_digest
            and changed_preserved_digest != preserved_wrapper_digest
        ):
            rep.ok("reject:who-metadata-near-match-and-examples-remain-in-digest")
        else:
            rep.bad(
                "reject:who-metadata-near-match-and-examples-remain-in-digest",
                f"base={base_review_digest}; preserved={preserved_wrapper_digest}; "
                f"changed={changed_preserved_digest}",
            )
        preserved_path = write_review_variant(
            "who-metadata-preserved", preserved_wrapper_events
        )
        _record_same_exit(
            rep,
            "reject:who-metadata-near-match-invalidates-context",
            py,
            rs,
            (
                "plan",
                "--fixture",
                preserved_path,
                "--landing-context",
                review_context_path,
            ),
            2,
        )
        appended_objection = dict(review_fixture)
        appended_objection_pr = dict(review_pr)
        appended_objection_events = [dict(event) for event in review_events]
        appended_objection_events[1]["body"] = (
            f"{appended_objection_events[1]['body']}\n"
            "Do not land: the race remains."
        )
        appended_objection_pr["review_events"] = appended_objection_events
        appended_objection["prs"] = [appended_objection_pr]
        appended_objection_path = os.path.join(tmp, "appended-review-objection.json")
        with open(appended_objection_path, "w", encoding="utf-8") as handle:
            json.dump(appended_objection, handle)
        if review_digest(
            review_head, "CHANGES_REQUESTED", appended_objection_events
        ) != review_digest(review_head, "CHANGES_REQUESTED", review_events):
            rep.ok("reject:appended-objection-changes-review-digest")
        else:
            rep.bad(
                "reject:appended-objection-changes-review-digest",
                "appended substantive objection was absent from the digest",
            )
        _record_same_exit(
            rep,
            "reject:appended-objection-invalidates-resolution",
            py,
            rs,
            (
                "plan",
                "--fixture",
                appended_objection_path,
                "--landing-context",
                review_context_path,
            ),
            2,
        )
        appended_uncontexted, _appended_uncontexted_rs = _record_exact(
            rep,
            "reject:appended-objection-remains-visible",
            py,
            rs,
            ("plan", "--fixture", appended_objection_path, "--format", "json"),
            expected=0,
        )
        appended_held, appended_action = review_disposition(appended_uncontexted, 394)
        if appended_held is True and appended_action == "wait":
            rep.ok("reject:appended-objection-keeps-changes-requested-hold")
        else:
            rep.bad(
                "reject:appended-objection-keeps-changes-requested-hold",
                f"held={appended_held}; action={appended_action!r}",
            )

        missing_author_fixture = dict(review_fixture)
        missing_author_pr = dict(review_pr)
        missing_author_events = [dict(event) for event in review_events]
        missing_author_events[2].pop("author")
        missing_author_pr["review_events"] = missing_author_events
        missing_author_fixture["prs"] = [missing_author_pr]
        missing_author_path = os.path.join(tmp, "review-metadata-missing-author.json")
        with open(missing_author_path, "w", encoding="utf-8") as handle:
            json.dump(missing_author_fixture, handle)
        _record_exact(
            rep,
            "accept:review-metadata-missing-author",
            py,
            rs,
            (
                "plan",
                "--fixture",
                missing_author_path,
                "--landing-context",
                review_context_path,
            ),
            expected=0,
        )

        changed_permission = dict(review_fixture)
        changed_permission_pr = dict(review_pr)
        changed_permission_events = [dict(event) for event in review_events]
        changed_permission_events[1]["retirement_actor_permission"] = "maintain"
        changed_permission_pr["review_events"] = changed_permission_events
        changed_permission["prs"] = [changed_permission_pr]
        changed_permission_path = os.path.join(
            tmp, "changed-retirement-actor-permission.json"
        )
        with open(changed_permission_path, "w", encoding="utf-8") as handle:
            json.dump(changed_permission, handle)
        _record_same_exit(
            rep,
            "reject:changed-retirement-actor-permission",
            py,
            rs,
            (
                "plan",
                "--fixture",
                changed_permission_path,
                "--landing-context",
                review_context_path,
            ),
            2,
        )

        for label, remove_author in (
            ("missing-permission", False),
            ("missing-author-and-permission", True),
        ):
            unavailable = dict(review_fixture)
            unavailable_pr = dict(review_pr)
            unavailable_events = [dict(event) for event in review_events]
            unavailable_events[1].pop("retirement_actor_permission")
            if remove_author:
                unavailable_events[1].pop("author")
            unavailable_pr["review_events"] = unavailable_events
            unavailable["prs"] = [unavailable_pr]
            unavailable_path = os.path.join(tmp, f"retirement-{label}.json")
            with open(unavailable_path, "w", encoding="utf-8") as handle:
                json.dump(unavailable, handle)
            outcome, _rust_outcome = _record_exact(
                rep,
                f"accept:retirement-{label}-remains-visible",
                py,
                rs,
                ("plan", "--fixture", unavailable_path, "--format", "json"),
                expected=0,
            )
            unavailable_held, unavailable_action = review_disposition(outcome, 394)
            if unavailable_held is True and unavailable_action == "wait":
                rep.ok(f"accept:retirement-{label}-does-not-clear-objection")
            else:
                rep.bad(
                    f"accept:retirement-{label}-does-not-clear-objection",
                    f"held={unavailable_held}; action={unavailable_action!r}",
                )

        read_only = dict(review_fixture)
        read_only_pr = dict(review_pr)
        read_only_events = [dict(event) for event in review_events]
        read_only_events[1]["retirement_actor_permission"] = "read"
        read_only_pr["review_events"] = read_only_events
        read_only["prs"] = [read_only_pr]
        read_only_path = os.path.join(tmp, "retirement-permission-read-only.json")
        with open(read_only_path, "w", encoding="utf-8") as handle:
            json.dump(read_only, handle)
        _record_same_exit(
            rep,
            "reject:retirement-permission-read-only",
            py,
            rs,
            ("plan", "--fixture", read_only_path),
            2,
        )

        for label, event_author in (
            ("departed-issuer", "departed-reviewer"),
            ("hostname-issuer", "devbig014"),
        ):
            named = dict(review_fixture)
            named_pr = dict(review_pr)
            named_events = [dict(event) for event in review_events]
            named_events[1]["author"] = event_author
            named_events[1]["body"] = str(named_events[1]["body"]).replace(
                "BY release-authority", "BY someone-else"
            )
            named_pr["review_events"] = named_events
            named["prs"] = [named_pr]
            named_path = os.path.join(tmp, f"retirement-{label}.json")
            with open(named_path, "w", encoding="utf-8") as handle:
                json.dump(named, handle)
            named_context_path = os.path.join(tmp, f"retirement-{label}-context.json")
            with open(named_context_path, "w", encoding="utf-8") as handle:
                json.dump(
                    {
                        "prs": [
                            {
                                "pr": 394,
                                "head_sha": review_head,
                                "review_objections_resolved": True,
                                "review_evidence_digest": review_digest(
                                    review_head, "CHANGES_REQUESTED", named_events
                                ),
                            }
                        ]
                    },
                    handle,
                )
            _record_exact(
                rep,
                f"accept:retirement-{label}",
                py,
                rs,
                (
                    "plan",
                    "--fixture",
                    named_path,
                    "--landing-context",
                    named_context_path,
                ),
                expected=0,
            )

        multiple_agents = dict(review_fixture)
        multiple_agents_pr = dict(review_pr)
        multiple_agents_pr["labels"] = ["agent:departed", "agent:replacement"]
        multiple_agents["prs"] = [multiple_agents_pr]
        multiple_agents_path = os.path.join(tmp, "multiple-agent-labels.json")
        with open(multiple_agents_path, "w", encoding="utf-8") as handle:
            json.dump(multiple_agents, handle)
        multiple_agents_outcome, _multiple_agents_rust = _record_exact(
            rep,
            "accept:multiple-agent-labels-are-metadata",
            py,
            rs,
            (
                "plan",
                "--fixture",
                multiple_agents_path,
                "--landing-context",
                review_context_path,
                "--format",
                "json",
            ),
            expected=0,
        )
        multiple_agents_ok, multiple_agents_payload = _parsed_json(
            multiple_agents_outcome.stdout
        )
        multiple_agent_nodes = (
            multiple_agents_payload.get("nodes")
            if multiple_agents_ok and isinstance(multiple_agents_payload, dict)
            else None
        )
        multiple_agent_value = (
            multiple_agent_nodes[0].get("assigned_agent")
            if isinstance(multiple_agent_nodes, list)
            and len(multiple_agent_nodes) == 1
            and isinstance(multiple_agent_nodes[0], dict)
            else "invalid"
        )
        if multiple_agent_value is None:
            rep.ok("accept:multiple-agent-labels-do-not-assign-authority")
        else:
            rep.bad(
                "accept:multiple-agent-labels-do-not-assign-authority",
                f"assigned_agent={multiple_agent_value!r}",
            )

        # Observed #2950 host shape: issue comments only, no native reviews, and an
        # empty aggregate reviewDecision. Stable ids, heads, and times are preserved.
        live_head = "58bf4f5c17880395400ab3cb6482d0de2ee03a84"
        refused_head = "c3a85da26110fd8dfc31433c8d4109726127703b"
        repaired_head = "2a18b7bea06cf66f5cf4a973466afa506c0bd2a4"
        live_shape_events: list[dict[str, object]] = [
            {
                "kind": "issue-comment",
                "identity": "comment-5545346256",
                "author": "rrnewton",
                "state": "ACTIVE",
                "head_sha": "",
                "created_at": "2026-09-04T19:14:42Z",
                "updated_at": "2026-09-04T19:14:42Z",
                "last_edited_at": "",
                "body": (
                    "[team, review-cpuid, unresolved, build-host, role=reviewer]\n"
                    "# Exact-head adversarial review\n\n"
                    f"CHANGES-REQUESTED-AT: claude {refused_head}\n\n"
                    "Major: the backend disagreement remains."
                ),
                "retirement_actor_permission": "",
            },
            {
                "kind": "issue-comment",
                "identity": "comment-5552679512",
                "author": "rrnewton",
                "state": "ACTIVE",
                "head_sha": "",
                "created_at": "2026-09-05T15:03:48Z",
                "updated_at": "2026-09-05T15:03:48Z",
                "last_edited_at": "",
                "body": (
                    "[team, review-cpuid, unresolved, build-host, role=reviewer]\n"
                    "# Reassessment at the repaired head\n\n"
                    f"CHANGES-REQUESTED-WITHDRAWN-AT: claude {repaired_head}\n\n"
                    f"APPROVED-AT: claude {repaired_head}\n\n"
                    "It is retired: the objection is met on the mechanism."
                ),
                "retirement_actor_permission": "",
            },
            {
                "kind": "issue-comment",
                "identity": "comment-5552753110",
                "author": "rrnewton",
                "state": "ACTIVE",
                "head_sha": "",
                "created_at": "2026-09-05T15:16:26Z",
                "updated_at": "2026-09-05T15:16:26Z",
                "last_edited_at": "",
                "body": (
                    "[team, review-cpuid, unresolved, build-host, role=reviewer]\n"
                    "# Rebind to the rebased head\n\n"
                    f"APPROVED-AT: claude {live_head}\n\n"
                    "My refusal from the earlier head stays retired."
                ),
                "retirement_actor_permission": "",
            },
        ]
        assert all("RETIRES" not in str(event["body"]) for event in live_shape_events)
        assert not any(event["kind"] == "review" for event in live_shape_events)
        live_shape_pr = dict(review_pr)
        live_shape_pr["head_sha"] = live_head
        live_shape_pr["review_decision"] = ""
        live_shape_pr["review_snapshot_review_decision"] = ""
        live_shape_pr["review_events"] = live_shape_events
        live_shape_fixture = dict(review_fixture)
        live_shape_fixture["prs"] = [live_shape_pr]
        live_shape_path = os.path.join(tmp, "chronological-withdrawal.json")
        with open(live_shape_path, "w", encoding="utf-8") as handle:
            json.dump(live_shape_fixture, handle)
        live_shape_context_path = os.path.join(
            tmp, "chronological-withdrawal-context.json"
        )
        with open(live_shape_context_path, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "prs": [
                        {
                            "pr": 394,
                            "head_sha": live_head,
                            "review_objections_resolved": True,
                            "review_evidence_digest": review_digest(
                                live_head, "", live_shape_events
                            ),
                        }
                    ]
                },
                handle,
            )
        live_shape_outcome, _live_shape_rust = _record_exact(
            rep,
            "accept:chronological-withdrawal-without-retires",
            py,
            rs,
            (
                "plan",
                "--fixture",
                live_shape_path,
                "--landing-context",
                live_shape_context_path,
                "--format",
                "json",
            ),
            expected=0,
        )
        live_shape_held, live_shape_action = review_disposition(live_shape_outcome, 394)
        if live_shape_held is False and live_shape_action == "land-now":
            rep.ok("accept:chronological-withdrawal-clears-held-state")
        else:
            rep.bad(
                "accept:chronological-withdrawal-clears-held-state",
                f"held={live_shape_held}; action={live_shape_action!r}",
            )

        live_uncontexted, _live_uncontexted_rs = _record_exact(
            rep,
            "reject:chronological-withdrawal-needs-resolution-context",
            py,
            rs,
            ("plan", "--fixture", live_shape_path, "--format", "json"),
            expected=0,
        )
        live_uncontexted_held, live_uncontexted_action = review_disposition(
            live_uncontexted, 394
        )
        if live_uncontexted_held is True and live_uncontexted_action == "wait":
            rep.ok("reject:chronological-withdrawal-uncontexted-hold")
        else:
            rep.bad(
                "reject:chronological-withdrawal-uncontexted-hold",
                f"held={live_uncontexted_held}; action={live_uncontexted_action!r}",
            )

        live_false_context_path = os.path.join(
            tmp, "chronological-withdrawal-false-context.json"
        )
        with open(live_false_context_path, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "prs": [
                        {
                            "pr": 394,
                            "head_sha": live_head,
                            "review_objections_resolved": False,
                        }
                    ]
                },
                handle,
            )
        live_false, _live_false_rs = _record_exact(
            rep,
            "reject:chronological-withdrawal-false-context",
            py,
            rs,
            (
                "plan",
                "--fixture",
                live_shape_path,
                "--landing-context",
                live_false_context_path,
                "--format",
                "json",
            ),
            expected=0,
        )
        live_false_held, live_false_action = review_disposition(live_false, 394)
        if live_false_held is True and live_false_action == "wait":
            rep.ok("reject:chronological-withdrawal-false-context-hold")
        else:
            rep.bad(
                "reject:chronological-withdrawal-false-context-hold",
                f"held={live_false_held}; action={live_false_action!r}",
            )

        live_metadata_events = [dict(event) for event in live_shape_events]
        for event in live_metadata_events:
            body = str(event["body"]).replace(
                "[team, review-cpuid, unresolved, build-host, role=reviewer]",
                "[team, departed.reviewer_v2, old-session, other-host, role=reviewer]",
            )
            event["body"] = re.sub(
                r"(?m)^((?:CHANGES-REQUESTED-WITHDRAWN-AT|"
                r"CHANGES-REQUESTED-AT|APPROVED-AT):\s*"
                r"(?:claude|codex)\s+[0-9a-f]{40})$",
                r"\1 BY departed-reviewer",
                body,
            )
        if review_digest(live_head, "", live_metadata_events) == review_digest(
            live_head, "", live_shape_events
        ):
            rep.ok("accept:chronological-withdrawal-attribution-is-metadata")
        else:
            rep.bad(
                "accept:chronological-withdrawal-attribution-is-metadata",
                "disclosure or BY metadata changed the review digest",
            )
        live_metadata_pr = dict(live_shape_pr)
        live_metadata_pr["review_events"] = live_metadata_events
        live_metadata_fixture = dict(live_shape_fixture)
        live_metadata_fixture["prs"] = [live_metadata_pr]
        live_metadata_path = os.path.join(
            tmp, "chronological-withdrawal-attribution-metadata.json"
        )
        with open(live_metadata_path, "w", encoding="utf-8") as handle:
            json.dump(live_metadata_fixture, handle)
        live_metadata_outcome, _live_metadata_rs = _record_exact(
            rep,
            "accept:chronological-withdrawal-attribution-metadata",
            py,
            rs,
            (
                "plan",
                "--fixture",
                live_metadata_path,
                "--landing-context",
                live_shape_context_path,
                "--format",
                "json",
            ),
            expected=0,
        )
        live_metadata_held, live_metadata_action = review_disposition(
            live_metadata_outcome, 394
        )
        if live_metadata_held is False and live_metadata_action == "land-now":
            rep.ok("accept:chronological-withdrawal-metadata-keeps-decision")
        else:
            rep.bad(
                "accept:chronological-withdrawal-metadata-keeps-decision",
                f"held={live_metadata_held}; action={live_metadata_action!r}",
            )

        stale_live_context_path = os.path.join(
            tmp, "chronological-withdrawal-stale-head-context.json"
        )
        with open(stale_live_context_path, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "prs": [
                        {
                            "pr": 394,
                            "head_sha": repaired_head,
                            "review_objections_resolved": True,
                            "review_evidence_digest": review_digest(
                                live_head, "", live_shape_events
                            ),
                        }
                    ]
                },
                handle,
            )
        _record_same_exit(
            rep,
            "reject:chronological-withdrawal-stale-head",
            py,
            rs,
            (
                "plan",
                "--fixture",
                live_shape_path,
                "--landing-context",
                stale_live_context_path,
            ),
            2,
        )

        chronology_cases: tuple[tuple[str, list[dict[str, object]]], ...] = (
            (
                "earlier-than-objection",
                [
                    *live_shape_events,
                    {
                        "kind": "review-comment",
                        "identity": "later-objection",
                        "author": "another-reviewer",
                        "state": "ACTIVE",
                        "head_sha": "",
                        "created_at": "2026-09-05T15:17:00Z",
                        "updated_at": "2026-09-05T15:17:00Z",
                        "last_edited_at": "",
                        "body": "later substantive objection",
                        "retirement_actor_permission": "",
                    },
                ],
            ),
            (
                "malformed-later-approval",
                [
                    *live_shape_events[:-1],
                    {
                        **live_shape_events[-1],
                        "body": "APPROVED-AT: claude 58bf4f5c",
                    },
                ],
            ),
        )
        for label, chronology_events in chronology_cases:
            changed_live = dict(live_shape_fixture)
            changed_live_pr = dict(live_shape_pr)
            changed_live_pr["review_events"] = chronology_events
            changed_live["prs"] = [changed_live_pr]
            changed_live_path = os.path.join(tmp, f"chronological-{label}.json")
            with open(changed_live_path, "w", encoding="utf-8") as handle:
                json.dump(changed_live, handle)
            _record_same_exit(
                rep,
                f"reject:chronological-withdrawal-{label}",
                py,
                rs,
                (
                    "plan",
                    "--fixture",
                    changed_live_path,
                    "--landing-context",
                    live_shape_context_path,
                ),
                2,
            )

        no_later_pr = dict(live_shape_pr)
        no_later_pr["review_events"] = live_shape_events[:1]
        no_later_fixture = dict(live_shape_fixture)
        no_later_fixture["prs"] = [no_later_pr]
        no_later_path = os.path.join(tmp, "chronological-no-later-artifact.json")
        with open(no_later_path, "w", encoding="utf-8") as handle:
            json.dump(no_later_fixture, handle)
        no_later_outcome, _no_later_rs = _record_exact(
            rep,
            "reject:chronological-withdrawal-no-later-artifact-held",
            py,
            rs,
            ("plan", "--fixture", no_later_path, "--format", "json"),
            expected=0,
        )
        no_later_held, no_later_action = review_disposition(no_later_outcome, 394)
        if no_later_held is True and no_later_action == "wait":
            rep.ok("reject:chronological-withdrawal-no-later-artifact-is-held")
        else:
            rep.bad(
                "reject:chronological-withdrawal-no-later-artifact-is-held",
                f"held={no_later_held}; action={no_later_action!r}",
            )
        no_later_digest = review_digest(live_head, "", live_shape_events[:1])
        malformed_refusal_digests: list[str] = []
        for label, by_suffix, canonical in (
            ("hyphen", " BY departed-reviewer", True),
            ("underscore", " BY departed_reviewer", True),
            ("dot", " BY departed.reviewer", True),
            ("slash", " BY departed/reviewer", False),
            ("space", " BY departed reviewer", False),
            ("missing-agent", " BY", False),
            ("colon", " BY: departed.reviewer", False),
            ("compact", " BY/departed.reviewer", False),
        ):
            refusal_events = [dict(live_shape_events[0])]
            marker = f"CHANGES-REQUESTED-AT: claude {refused_head}"
            refusal_events[0]["body"] = str(refusal_events[0]["body"]).replace(
                marker, f"{marker}{by_suffix}"
            )
            refusal_digest = review_digest(live_head, "", refusal_events)
            if canonical:
                if refusal_digest == no_later_digest:
                    rep.ok(f"accept:comment-refusal-by-{label}-is-metadata")
                else:
                    rep.bad(
                        f"accept:comment-refusal-by-{label}-is-metadata",
                        f"base={no_later_digest}; changed={refusal_digest}",
                    )
            else:
                malformed_refusal_digests.append(refusal_digest)
                if refusal_digest != no_later_digest:
                    rep.ok(f"reject:comment-refusal-by-{label}-bytes-preserved")
                else:
                    rep.bad(
                        f"reject:comment-refusal-by-{label}-bytes-preserved",
                        "malformed BY text was removed from the digest",
                    )
            refusal_fixture = dict(no_later_fixture)
            refusal_pr = dict(no_later_pr)
            refusal_pr["review_events"] = refusal_events
            refusal_fixture["prs"] = [refusal_pr]
            refusal_path = os.path.join(tmp, f"comment-refusal-by-{label}.json")
            with open(refusal_path, "w", encoding="utf-8") as handle:
                json.dump(refusal_fixture, handle)
            refusal_outcome, _refusal_rs = _record_exact(
                rep,
                f"reject:comment-refusal-by-{label}-remains-held",
                py,
                rs,
                ("plan", "--fixture", refusal_path, "--format", "json"),
                expected=0,
            )
            refusal_held, refusal_action = review_disposition(refusal_outcome, 394)
            if refusal_held is True and refusal_action == "wait":
                rep.ok(f"reject:comment-refusal-by-{label}-decision-unchanged")
            else:
                rep.bad(
                    f"reject:comment-refusal-by-{label}-decision-unchanged",
                    f"held={refusal_held}; action={refusal_action!r}",
                )
        if len(set(malformed_refusal_digests)) == len(malformed_refusal_digests):
            rep.ok("reject:malformed-comment-by-mutations-change-digest")
        else:
            rep.bad(
                "reject:malformed-comment-by-mutations-change-digest",
                f"digests={malformed_refusal_digests}",
            )
        for whitespace_label, non_ascii_space in (
            ("nbsp", "\u00a0"),
            ("em-space", "\u2003"),
        ):
            marker = f"CHANGES-REQUESTED-AT: claude {refused_head}"
            for grammar, malformed_marker in (
                (
                    "before-marker",
                    f"{non_ascii_space}{marker}",
                ),
                (
                    "after-heading",
                    f"#{non_ascii_space}{marker}",
                ),
                (
                    "after-colon",
                    marker.replace(": ", f":{non_ascii_space}"),
                ),
                (
                    "after-lane",
                    marker.replace("claude ", f"claude{non_ascii_space}"),
                ),
            ):
                refusal_events = [dict(live_shape_events[0])]
                refusal_events[0]["body"] = str(refusal_events[0]["body"]).replace(
                    marker, malformed_marker
                )
                refusal_digest = review_digest(live_head, "", refusal_events)
                if refusal_digest != no_later_digest:
                    rep.ok(
                        f"reject:non-ascii-whitespace-{whitespace_label}-{grammar}-refusal-digest"
                    )
                else:
                    rep.bad(
                        f"reject:non-ascii-whitespace-{whitespace_label}-{grammar}-refusal-digest",
                        "non-ASCII whitespace was accepted as refusal syntax",
                    )
                refusal_fixture = dict(no_later_fixture)
                refusal_pr = dict(no_later_pr)
                refusal_pr["review_events"] = refusal_events
                refusal_fixture["prs"] = [refusal_pr]
                refusal_path = os.path.join(
                    tmp,
                    f"comment-refusal-{whitespace_label}-{grammar}.json",
                )
                with open(refusal_path, "w", encoding="utf-8") as handle:
                    json.dump(refusal_fixture, handle)
                refusal_outcome, _refusal_rs = _record_exact(
                    rep,
                    f"reject:non-ascii-whitespace-{whitespace_label}-{grammar}-refusal",
                    py,
                    rs,
                    ("plan", "--fixture", refusal_path, "--format", "json"),
                    expected=0,
                )
                refusal_held, refusal_action = review_disposition(
                    refusal_outcome, 394
                )
                if refusal_held is False and refusal_action == "land-now":
                    rep.ok(
                        f"reject:non-ascii-whitespace-{whitespace_label}-{grammar}-unrecognized"
                    )
                else:
                    rep.bad(
                        f"reject:non-ascii-whitespace-{whitespace_label}-{grammar}-unrecognized",
                        f"held={refusal_held}; action={refusal_action!r}",
                    )
        _record_same_exit(
            rep,
            "reject:chronological-withdrawal-no-later-artifact-invalidates-context",
            py,
            rs,
            (
                "plan",
                "--fixture",
                no_later_path,
                "--landing-context",
                live_shape_context_path,
            ),
            2,
        )

        mismatched_snapshot = dict(review_fixture)
        mismatched_pr = dict(review_pr)
        mismatched_pr["review_snapshot_head_sha"] = "b" * 40
        mismatched_snapshot["prs"] = [mismatched_pr]
        mismatch_path = os.path.join(tmp, "mismatched-review-snapshot-head.json")
        with open(mismatch_path, "w", encoding="utf-8") as handle:
            json.dump(mismatched_snapshot, handle)
        _record_same_exit(
            rep,
            "reject:review-snapshot-fetched-head-mismatch",
            py,
            rs,
            ("plan", "--fixture", mismatch_path),
            2,
        )

        for label, decision in (
            ("null", None),
            ("empty", ""),
            ("conflict", "APPROVED"),
        ):
            mismatched_decision = dict(review_fixture)
            mismatched_decision_pr = dict(review_pr)
            mismatched_decision_pr["review_snapshot_review_decision"] = decision
            mismatched_decision["prs"] = [mismatched_decision_pr]
            decision_path = os.path.join(
                tmp, f"mismatched-review-decision-{label}.json"
            )
            with open(decision_path, "w", encoding="utf-8") as handle:
                json.dump(mismatched_decision, handle)
            _record_same_exit(
                rep,
                f"reject:review-decision-{label}",
                py,
                rs,
                ("plan", "--fixture", decision_path),
                2,
            )

        missing_identity = dict(review_fixture)
        missing_identity_pr = dict(review_pr)
        missing_identity_pr["review_events"] = [{**review_events[0], "identity": ""}]
        missing_identity["prs"] = [missing_identity_pr]
        missing_identity_path = os.path.join(tmp, "missing-review-event-identity.json")
        with open(missing_identity_path, "w", encoding="utf-8") as handle:
            json.dump(missing_identity, handle)
        _record_same_exit(
            rep,
            "reject:missing-review-event-identity",
            py,
            rs,
            ("plan", "--fixture", missing_identity_path),
            2,
        )

        malformed_review_cases: list[tuple[str, dict[str, object]]] = []
        numeric_author = dict(review_events[0])
        numeric_author["author"] = 7
        malformed_review_cases.append(("numeric-author", numeric_author))
        null_created = dict(review_events[0])
        null_created["created_at"] = None
        malformed_review_cases.append(("null-created-at", null_created))
        numeric_last_edit = dict(review_events[0])
        numeric_last_edit["last_edited_at"] = 7
        malformed_review_cases.append(("numeric-last-edited-at", numeric_last_edit))
        missing_body = dict(review_events[0])
        missing_body.pop("body")
        malformed_review_cases.append(("missing-body", missing_body))
        alien_state = dict(review_events[0])
        alien_state["state"] = "ALIEN_STATE"
        malformed_review_cases.append(("alien-state", alien_state))
        for label, malformed_event in malformed_review_cases:
            malformed = dict(review_fixture)
            malformed_pr = dict(review_pr)
            malformed_pr["review_events"] = [malformed_event]
            malformed["prs"] = [malformed_pr]
            malformed_path = os.path.join(tmp, f"malformed-review-{label}.json")
            with open(malformed_path, "w", encoding="utf-8") as handle:
                json.dump(malformed, handle)
            _record_same_exit(
                rep,
                f"reject:malformed-review-{label}",
                py,
                rs,
                ("plan", "--fixture", malformed_path),
                2,
            )

        null_snapshot_head = dict(review_fixture)
        for label, malformed_unavailable in (
            ("null", None),
            ("integer", 0),
            ("string", "true"),
            ("array", []),
            ("object", {}),
        ):
            malformed_unavailable_fixture: dict[str, object] = {
                "repo": "OWNER/NAME",
                "base": "main",
                "prs": [
                    {
                        "number": 1,
                        "review_evidence_unavailable": malformed_unavailable,
                    }
                ],
            }
            malformed_unavailable_path = os.path.join(
                tmp, f"malformed-review-evidence-unavailable-{label}.json"
            )
            with open(malformed_unavailable_path, "w", encoding="utf-8") as handle:
                json.dump(malformed_unavailable_fixture, handle)
            _record_same_exit(
                rep,
                f"reject:review-evidence-unavailable-{label}",
                py,
                rs,
                ("plan", "--fixture", malformed_unavailable_path),
                2,
            )

        null_snapshot_pr = dict(review_pr)
        null_snapshot_pr["review_snapshot_head_sha"] = None
        null_snapshot_head["prs"] = [null_snapshot_pr]
        null_snapshot_path = os.path.join(tmp, "null-review-snapshot-head.json")
        with open(null_snapshot_path, "w", encoding="utf-8") as handle:
            json.dump(null_snapshot_head, handle)
        _record_same_exit(
            rep,
            "reject:null-review-snapshot-head",
            py,
            rs,
            ("plan", "--fixture", null_snapshot_path),
            2,
        )

        for flag, value in (
            ("--outage-min-prs", "-1"),
            ("--freshness-max-behind", "-1"),
            ("--freshness-max-behind", "9223372036854775808"),
            ("--prs", "0"),
            ("--prs", "9223372036854775808"),
        ):
            _record_same_exit(
                rep,
                f"reject:cli-domain/{flag}={value}",
                py,
                rs,
                ("plan", "--fixture", fixture_path, flag, value),
                2,
            )

        rng = random.Random(seed)
        conclusions = (
            ("COMPLETED", "SUCCESS"),
            ("IN_PROGRESS", ""),
            ("COMPLETED", "FAILURE"),
            ("COMPLETED", "CANCELLED"),
        )
        for index in range(rand_count):
            prs: list[object] = []
            conflicts: list[object] = []
            count = rng.randint(1, 8)
            for number in range(1, count + 1):
                status, conclusion = rng.choice(conclusions)
                gate_status, gate_conclusion = rng.choice(conclusions)
                checks: list[object] = [
                    {
                        "name": "CI",
                        "status": status,
                        "conclusion": conclusion,
                        "text": "assertion" if conclusion == "FAILURE" else "",
                    },
                    {
                        "name": "merge-gate",
                        "status": gate_status,
                        "conclusion": gate_conclusion,
                        "text": "stale" if gate_conclusion == "FAILURE" else "",
                    },
                ]
                prs.append(
                    {
                        "number": number,
                        "head_ref": f"feature-{number}",
                        "base_ref": (
                            f"feature-{number - 1}"
                            if number > 1 and rng.random() < 0.2
                            else "main"
                        ),
                        "title": f"random {index}/{number}",
                        "is_draft": rng.random() < 0.1,
                        "review_decision": rng.choice(("", "APPROVED", "REVIEW_REQUIRED")),
                        "additions": rng.randint(0, 100),
                        "deletions": rng.randint(0, 100),
                        "commits_behind": rng.randint(0, 4),
                        "labels": [f"priority:{rng.randint(0, 3)}"],
                        "changed_files": [f"src/{rng.randint(0, 4)}.rs"],
                        "checks": checks,
                    }
                )
                if number > 1 and rng.random() < 0.25:
                    conflicts.append(
                        {"a": rng.randint(1, number - 1), "b": number, "paths": ["src/shared.rs"]}
                    )
            random_fixture = {
                "repo": "R",
                "base": "main",
                "prs": prs,
                "conflicts": conflicts,
            }
            path = os.path.join(tmp, f"random-{index}.json")
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(random_fixture, handle)
            extra: tuple[str, ...] = (
                "--batch",
                "--freshness-max-behind",
                str(rng.randint(0, 3)),
                "--conflict-detector",
                rng.choice(("merge-tree", "file-overlap")),
            )
            _record_exact(
                rep,
                f"random:{index}/plan-json",
                py,
                rs,
                ("plan", "--fixture", path, "--format", "json", *extra),
            )

    if rep.failures:
        return divergence_verdict(tool, rep, f"{rep.checks} checks", frozenset())
    # No case here may skip; if one ever does, the default verdict refuses to call it a pass.
    return coverage_verdict(
        tool, rep, f"{rep.checks} graph, CI, randomized, and adversarial checks agree", frozenset()
    )


DIFFERENTIAL_TOOLS = (
    "dagrun",
    "cpuset-alloc",
    "tick-hub",
    "pr-landing-planner",
    "herdr-run",
    "herdr-agent",
    "agentctl",
)


def allowed_skip_kinds(flags: Sequence[str], environ: Mapping[str, str]) -> dict[str, str]:
    """Map each skip kind a lane declared acceptable to where it was declared.

    Kinds come from repeated or comma-separated ``--allow-skip`` values and from
    ``ALLOW_SKIP_ENV``. An unknown kind is refused rather than ignored, so a misspelled allowance
    cannot turn into an unnoticed full-coverage claim or an unnoticed partial one.
    """

    declared: list[tuple[str, str]] = [("--allow-skip", value) for value in flags]
    declared.append((ALLOW_SKIP_ENV, environ.get(ALLOW_SKIP_ENV, "")))
    sources: dict[str, str] = {}
    for source, raw in declared:
        for item in raw.split(","):
            kind = item.strip()
            if not kind:
                continue
            if kind not in SKIP_KINDS:
                raise ValueError(
                    f"{source}: unknown skip kind {kind!r}; expected one of "
                    f"{', '.join(sorted(SKIP_KINDS))}"
                )
            sources.setdefault(kind, source)
    return sources


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="py-vs-rs differential tester")
    parser.add_argument(
        "--tool",
        default="dagrun",
        choices=(*DIFFERENTIAL_TOOLS, "all"),
    )
    parser.add_argument(
        "--list-tools",
        action="store_true",
        help="list the independently selectable differential tools and exit",
    )
    parser.add_argument("--random", type=int, default=24, help="number of randomized fixtures")
    parser.add_argument("--seed", type=int, default=1234, help="RNG seed for randomized fixtures")
    parser.add_argument(
        "--allow-skip",
        action="append",
        default=[],
        metavar="KIND[,KIND]",
        help=(
            "declare a partial run: skipped checks of these kinds are still listed and never "
            "counted as passes, but no longer make the run fail. Without it every skip fails "
            f"the run. Kinds: {', '.join(sorted(SKIP_KINDS))}. {ALLOW_SKIP_ENV} adds kinds "
            "the same way"
        ),
    )
    ns = parser.parse_args(list(argv) if argv is not None else None)
    if ns.list_tools:
        for name in DIFFERENTIAL_TOOLS:
            print(name)
        return 0
    try:
        allowance = allowed_skip_kinds([str(value) for value in ns.allow_skip], os.environ)
    except ValueError as exc:
        parser.error(str(exc))
    allowed_skips = frozenset(allowance)
    if allowance:
        declared = ", ".join(f"{kind} (from {source})" for kind, source in sorted(allowance.items()))
        print(
            "cross: PARTIAL-coverage run declared; skips of these kinds are allowed, listed, and "
            f"never counted as passes: {declared}"
        )
    tool = str(ns.tool)
    rand_count = int(ns.random)
    seed = int(ns.seed)
    try:
        validation_jobs = _validation_jobs() if tool in ("dagrun", "all") else None
    except ValueError as exc:
        parser.error(str(exc))
    jobs = validation_jobs if validation_jobs is not None else DEFAULT_VALIDATION_JOBS
    runners: dict[str, Callable[[], int]] = {
        "dagrun": lambda: compare_dagrun(
            rand_count, seed, validation_jobs=jobs, allowed_skips=allowed_skips
        ),
        "cpuset-alloc": lambda: compare_cpuset_alloc(allowed_skips=allowed_skips),
        "tick-hub": lambda: compare_tick_hub(rand_count, seed),
        "pr-landing-planner": lambda: compare_pr_landing_planner(rand_count, seed),
        "herdr-run": lambda: compare_herdr_run(
            py_command_for("herdr-run"), rs_command("herdr-run"), allowed_skips=allowed_skips
        ),
        "herdr-agent": lambda: compare_herdr_agent(
            py_command_for("herdr-agent"), rs_command("herdr-agent")
        ),
        "agentctl": lambda: compare_agentctl(py_command_for("agentctl"), rs_command("agentctl")),
    }
    assert tuple(runners) == DIFFERENTIAL_TOOLS

    def run_tool(name: str) -> tuple[int, ToolCoverage]:
        recorded = len(_COVERAGE)
        status = runners[name]()
        if len(_COVERAGE) == recorded:
            # The herdr-agent and agentctl differentials print their own verdict and skip nothing
            # through Report, so their exit status is the whole of what they report here.
            return status, record_coverage(
                name, "OK" if status == 0 else "FAILED", None, allowed_skips
            )
        return status, _COVERAGE[-1]

    if tool != "all":
        return run_tool(tool)[0]
    results = [run_tool(name) for name in DIFFERENTIAL_TOOLS]
    line, status = coverage_aggregate(results)
    print(line)
    return status


if __name__ == "__main__":
    raise SystemExit(main())
