#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Shared mutation harness for the backend-parity identity-fixture family.

The family of backend-parity identity fixtures (rlimit_identity.c,
sched_getaffinity_identity.c, getcpu_identity.c, ...) all make the same claim:
"every backend observes the same value the golden ptrace reference does." That
claim is only worth anything if the fixture would actually FAIL when a backend
gets the value wrong. Historically each fixture hand-rolled its own
both-direction proof, and a hand-rolled proof is a chance to write one that
cannot fail -- the vacuous-test shape. (Measured on the reverie staging batch:
5 of 5 members had tests that passed WITHOUT exercising their mechanism, and 4
of those 5 hid a real product bug.)

This harness gives the whole family ONE verification, so a new member cannot
drift into vacuity. Each member supplies only:

  * its syscall           -- the fixture .c source, and
  * its divergence        -- the name(s) of the mutable field(s) it threads
                             through the parity_mutate_*() seam in parity_probe.h.

The harness then proves, for every member, BOTH directions:

  (a) plant a divergence   -- run the fixture with HERMIT_PARITY_MUTATE naming a
      -> assert FAILURE        field. The seam perturbs that field's observed
                               value, so the fixture's (exit status, stdout)
                               must diverge from the clean golden run. If it does
                               NOT diverge, the field is not actually load-bearing
                               and the fixture is VACUOUS -- the harness fails.
  (b) run clean            -- run the fixture unperturbed under a candidate
      -> assert PASS           backend. Its (exit status, stdout) must MATCH the
                               golden ptrace reference. A mismatch is a real
                               backend-parity defect.

"Parity means matching the GOLDEN PTRACE REFERENCE," so ptrace is the default
comparison target in hermit mode, not something each fixture re-states.

Two run modes:

  * Native self-test (default; C compiler only, no hermit build). The reference
    is a clean native execution; the mutation direction proves each declared
    field is load-bearing, and the clean direction proves the fixture's contract
    holds. This is the cheap CI guard that catches a vacuous family member on
    every PR without building hermit.
  * Hermit cross-backend (with --hermit). Adds the REAL parity check: each
    candidate backend's clean run must match the golden ptrace run, and a
    divergence planted in that backend must be caught. A backend that cannot run
    (e.g. KVM without /dev/kvm) is SKIPPED with a reported reason -- never a
    silent pass -- unless --require-backend makes the skip fatal.

Adding a family member is one registry entry below: source path + field names.
No bespoke verification code travels with the fixture.
"""

from __future__ import annotations

import argparse
import dataclasses
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
FIXTURES_DIR = SCRIPT_DIR / "fixtures"

# The golden reference backend. "Parity" is defined against this.
GOLDEN_BACKEND = "ptrace"
# Candidate backends checked for parity against the golden reference by default.
DEFAULT_CANDIDATES = ("dbi", "kvm")

# Guest run timeout (seconds) for a single fixture execution under hermit.
HERMIT_TIMEOUT_S = 60
NATIVE_TIMEOUT_S = 30


class HarnessError(Exception):
    """A harness configuration or contract error (not a fixture verdict)."""


@dataclasses.dataclass(frozen=True)
class FixtureSpec:
    """A single family member. Supplies ONLY its syscall and its divergence.

    source: fixture .c source path (relative to fixtures/ unless absolute).
    fields: the mutable field name(s) it threads through the parity_mutate_*()
            seam. Each is proven load-bearing independently.
    cflags: extra compile flags (e.g. ("-pthread",)); -D_GNU_SOURCE is always
            supplied per parity_probe.h's contract.
    """

    source: str
    fields: tuple[str, ...]
    cflags: tuple[str, ...] = ()

    def source_path(self) -> Path:
        candidate = Path(self.source)
        return candidate if candidate.is_absolute() else FIXTURES_DIR / candidate


# The family registry. Every backend-parity identity fixture lives here with its
# field(s); nothing else. A new member is one line.
FIXTURES: dict[str, FixtureSpec] = {
    "rlimit_identity": FixtureSpec(
        source="rlimit_identity.c",
        fields=("nofile",),
    ),
    "sched_getaffinity_identity": FixtureSpec(
        source="sched_getaffinity_identity.c",
        fields=("affinity_count",),
    ),
}


@dataclasses.dataclass(frozen=True)
class Observation:
    """The guest-visible result of one fixture execution: what parity compares."""

    exit_status: int
    stdout: bytes

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, Observation):
            return NotImplemented
        return self.exit_status == other.exit_status and self.stdout == other.stdout

    def is_empty(self) -> bool:
        """True when this run emitted no identity payload at all.

        Two empty observations compare EQUAL, so without this an
        observation-free run reports parity. That is the vacuity that let a
        vdso fixture go green by emitting no bytes: nothing was compared, and
        nothing-vs-nothing matched.
        """
        return not self.stdout.strip()

    def summary(self) -> str:
        text = self.stdout.decode("utf-8", "replace").strip().replace("\n", " | ")
        return f"exit={self.exit_status} stdout={text!r}"


# ---------------------------------------------------------------------------
# Compilation
# ---------------------------------------------------------------------------


def compile_fixture(spec: FixtureSpec, output: Path) -> Path:
    """Compile a fixture with the shared flags (mirrors run_matrix.py)."""
    compiler = shutil.which(os.environ.get("CC", "cc"))
    if compiler is None:
        raise HarnessError("C compiler unavailable (set CC or install cc)")
    source = spec.source_path()
    if not source.is_file():
        raise HarnessError(f"fixture source missing: {source}")
    command = [
        compiler,
        "-O2",
        "-g",
        "-std=c11",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-D_GNU_SOURCE",
        f"-I{FIXTURES_DIR}",
        *spec.cflags,
        str(source),
        "-o",
        str(output),
    ]
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise HarnessError(
            f"fixture compilation failed: {command!r}\n{result.stdout}{result.stderr}"
        )
    return output


# Fields the fixture actually threads through the mutation seam, parsed from its
# source. Used to guard against a fixture growing an undeclared field: a field
# that is mutated but not registered would go unproven; a field registered but
# not mutated would be inert. Either is a drift the family must not permit.
_MUTATE_CALL_RE = re.compile(r"parity_mutate_(?:u64|i64|str)\s*\(\s*\"([^\"]+)\"")


def source_declared_fields(spec: FixtureSpec) -> set[str]:
    text = spec.source_path().read_text(encoding="utf-8")
    return set(_MUTATE_CALL_RE.findall(text))


# ---------------------------------------------------------------------------
# Execution
# ---------------------------------------------------------------------------


def _run(command: list[str], env: dict[str, str], timeout: int) -> Observation | None:
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        stdin=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        stdout, _ = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        return None
    return Observation(process.returncode, stdout)


def observe_native(binary: Path, mutate: str | None) -> Observation | None:
    """Run the compiled fixture directly, optionally planting a mutation."""
    env = dict(os.environ)
    env.pop("HERMIT_PARITY_MUTATE", None)
    if mutate is not None:
        env["HERMIT_PARITY_MUTATE"] = mutate
    return _run([str(binary)], env, NATIVE_TIMEOUT_S)


def _hermit_command(hermit: Path, backend: str, binary: Path, mutate: str | None) -> list[str]:
    command = [str(hermit), "run"]
    if backend != GOLDEN_BACKEND:
        command.extend(["--backend", backend])
    command.extend(["--strict", "--base-env=minimal", "--max-timeslice=disabled", "--tmp=/tmp"])
    if mutate is not None:
        # --base-env=minimal strips the ambient env, so the mutation must be
        # passed through explicitly for the guest to observe it.
        command.append(f"--env=HERMIT_PARITY_MUTATE={mutate}")
    command.extend(["--", str(binary)])
    return command


def observe_hermit(
    hermit: Path, backend: str, binary: Path, mutate: str | None
) -> Observation | None:
    """Run the fixture under a hermit backend, optionally planting a mutation."""
    env = dict(os.environ)
    env.pop("HERMIT_PARITY_MUTATE", None)  # only the guest, via --env, should see it
    return _run(_hermit_command(hermit, backend, binary, mutate), env, HERMIT_TIMEOUT_S)


def backend_available(hermit: Path, backend: str) -> tuple[bool, str]:
    """Smoke-test a candidate backend with a trivial guest."""
    if backend == GOLDEN_BACKEND:
        return True, ""
    command = [str(hermit), "run", "--backend", backend, "--base-env=minimal", "--", "/bin/true"]
    result = _run(command, dict(os.environ), HERMIT_TIMEOUT_S)
    if result is None:
        return False, "smoke test timed out"
    if result.exit_status != 0:
        detail = result.stdout.decode("utf-8", "replace").strip()[-200:]
        return False, f"smoke exit {result.exit_status}: {detail}"
    return True, ""


# ---------------------------------------------------------------------------
# Verdict accumulation
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class Report:
    checks: int = 0
    failures: list[str] = dataclasses.field(default_factory=list)
    skips: list[str] = dataclasses.field(default_factory=list)

    def ok(self, message: str) -> None:
        self.checks += 1
        print(f"  PASS {message}")

    def fail(self, message: str) -> None:
        self.checks += 1
        self.failures.append(message)
        print(f"  FAIL {message}")

    def skip(self, message: str) -> None:
        self.skips.append(message)
        print(f"  SKIP {message}")


def require_divergence(
    report: Report, label: str, golden: Observation | None, mutated: Observation | None
) -> None:
    """Assert that a planted mutation was CAUGHT (mutated diverges from golden)."""
    if golden is None or mutated is None:
        report.fail(f"{label}: run timed out (golden={golden}, mutated={mutated})")
        return
    if golden.is_empty() and mutated.is_empty():
        report.fail(
            f"{label}: VACUOUS -- neither run emitted an identity payload, so "
            f"there was nothing a mutation could perturb"
        )
        return
    if mutated == golden:
        report.fail(
            f"{label}: VACUOUS -- mutation changed nothing; field is not "
            f"load-bearing (both {golden.summary()})"
        )
    else:
        report.ok(f"{label}: divergence caught (golden {golden.summary()} != mutated {mutated.summary()})")


def require_parity(
    report: Report, label: str, golden: Observation | None, candidate: Observation | None
) -> None:
    """Assert that a clean candidate run MATCHES the golden ptrace reference."""
    if golden is None or candidate is None:
        report.fail(f"{label}: run timed out (golden={golden}, candidate={candidate})")
        return
    # NON-VACUITY leg. Checked BEFORE equality, because empty == empty is the
    # exact shape that reports success while comparing nothing.
    if golden.is_empty() or candidate.is_empty():
        report.fail(
            f"{label}: VACUOUS -- no identity payload to compare "
            f"(golden {golden.summary()}, candidate {candidate.summary()}); "
            f"a run that emits nothing must not report parity"
        )
        return
    if candidate == golden:
        report.ok(f"{label}: parity with golden ({golden.summary()})")
    else:
        report.fail(
            f"{label}: PARITY BREAK -- golden {golden.summary()} != candidate {candidate.summary()}"
        )


# ---------------------------------------------------------------------------
# Per-fixture drivers
# ---------------------------------------------------------------------------


def check_declared_fields(report: Report, name: str, spec: FixtureSpec) -> None:
    declared = set(spec.fields)
    in_source = source_declared_fields(spec)
    missing = in_source - declared
    inert = declared - in_source
    if missing:
        report.fail(
            f"{name}: field(s) {sorted(missing)} mutated in source but not "
            f"registered -- they would go unproven"
        )
    if inert:
        report.fail(
            f"{name}: registered field(s) {sorted(inert)} never appear in the "
            f"mutation seam -- they are inert"
        )
    if not missing and not inert:
        report.ok(f"{name}: declared fields {sorted(declared)} match the source seam exactly")


def run_native(report: Report, name: str, binary: Path, spec: FixtureSpec) -> None:
    print(f"[native] {name}")
    # (b) run clean -> assert PASS (the fixture's own contract holds).
    clean = observe_native(binary, mutate=None)
    if clean is None:
        report.fail(f"{name} [native]: clean run timed out")
        return
    if clean.exit_status != 0:
        report.fail(f"{name} [native]: clean contract FAILED ({clean.summary()})")
        return
    if not clean.stdout.strip():
        report.fail(f"{name} [native]: clean run emitted no identity line")
        return
    report.ok(f"{name} [native]: clean contract holds ({clean.summary()})")
    # (a) plant a divergence per field -> assert FAILURE is caught.
    for field in spec.fields:
        mutated = observe_native(binary, mutate=field)
        require_divergence(report, f"{name} [native] mutate({field})", clean, mutated)


def run_hermit(
    report: Report,
    name: str,
    binary: Path,
    spec: FixtureSpec,
    hermit: Path,
    candidates: tuple[str, ...],
    require_backend: bool,
) -> None:
    print(f"[hermit] {name}")
    # Golden ptrace reference: the default comparison target.
    golden = observe_hermit(hermit, GOLDEN_BACKEND, binary, mutate=None)
    if golden is None or golden.exit_status != 0:
        report.fail(
            f"{name} [hermit/{GOLDEN_BACKEND}]: golden reference did not pass "
            f"({golden.summary() if golden else 'timeout'})"
        )
        return
    if golden.is_empty():
        report.fail(
            f"{name} [hermit/{GOLDEN_BACKEND}]: VACUOUS -- golden reference "
            f"emitted no identity line, so every candidate that also emits "
            f"nothing would report parity against it"
        )
        return
    report.ok(f"{name} [hermit/{GOLDEN_BACKEND}]: golden reference ({golden.summary()})")

    # Seam works through hermit + --env passthrough (prove the mutation is
    # observable under the golden backend before trusting it as a probe).
    for field in spec.fields:
        mutated = observe_hermit(hermit, GOLDEN_BACKEND, binary, mutate=field)
        require_divergence(
            report, f"{name} [hermit/{GOLDEN_BACKEND}] mutate({field})", golden, mutated
        )

    for backend in candidates:
        if backend == GOLDEN_BACKEND:
            continue
        available, reason = backend_available(hermit, backend)
        if not available:
            message = f"{name} [hermit/{backend}]: backend unavailable ({reason})"
            if require_backend:
                report.fail(message)
            else:
                report.skip(message)
            continue
        # (b) clean candidate run -> assert parity with golden ptrace.
        clean = observe_hermit(hermit, backend, binary, mutate=None)
        require_parity(report, f"{name} [hermit/{backend}] clean", golden, clean)
        # (a) plant a divergence in this backend -> assert it is caught.
        for field in spec.fields:
            mutated = observe_hermit(hermit, backend, binary, mutate=field)
            require_divergence(
                report, f"{name} [hermit/{backend}] mutate({field})", golden, mutated
            )


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--hermit",
        type=Path,
        default=None,
        help="path to the hermit binary; enables cross-backend parity checks",
    )
    parser.add_argument(
        "--backend",
        action="append",
        dest="backends",
        default=None,
        help=f"candidate backend to check against golden {GOLDEN_BACKEND} "
        f"(repeatable; default {','.join(DEFAULT_CANDIDATES)})",
    )
    parser.add_argument(
        "--fixture",
        action="append",
        dest="fixtures",
        default=None,
        help="restrict to named fixture(s) (repeatable; default all)",
    )
    parser.add_argument(
        "--native-only",
        action="store_true",
        help="skip hermit cross-backend checks even if --hermit is given",
    )
    parser.add_argument(
        "--require-backend",
        action="store_true",
        help="treat an unavailable candidate backend as a failure, not a skip",
    )
    parser.add_argument(
        "--keep",
        action="store_true",
        help="keep compiled fixture binaries for inspection",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)

    selected = args.fixtures or list(FIXTURES)
    unknown = [name for name in selected if name not in FIXTURES]
    if unknown:
        raise HarnessError(f"unknown fixture(s): {unknown}; known: {sorted(FIXTURES)}")

    candidates = tuple(args.backends) if args.backends else DEFAULT_CANDIDATES

    report = Report()
    workdir = Path(tempfile.mkdtemp(prefix="parity-mutation-"))
    try:
        for name in selected:
            spec = FIXTURES[name]
            check_declared_fields(report, name, spec)
            binary = compile_fixture(spec, workdir / name)
            run_native(report, name, binary, spec)
            if args.hermit and not args.native_only:
                if not args.hermit.is_file():
                    report.fail(f"hermit binary not found: {args.hermit}")
                else:
                    run_hermit(
                        report,
                        name,
                        binary,
                        spec,
                        args.hermit,
                        candidates,
                        args.require_backend,
                    )
    finally:
        if not args.keep:
            shutil.rmtree(workdir, ignore_errors=True)

    print()
    print(
        f"parity-mutation: {report.checks} checks, "
        f"{len(report.failures)} failed, {len(report.skips)} skipped"
    )
    for skipped in report.skips:
        print(f"  skipped: {skipped}")
    for failed in report.failures:
        print(f"  failed:  {failed}")
    return 1 if report.failures else 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except HarnessError as error:
        print(f"parity-mutation: {error}", file=sys.stderr)
        sys.exit(2)
