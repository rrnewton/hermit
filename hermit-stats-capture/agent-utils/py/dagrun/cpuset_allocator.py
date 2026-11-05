#!/usr/bin/env python3
"""Stateful, hard process-tree CPU allocation for benchmark isolation.

The allocator combines a durable, ``flock``-serialized reservation ledger with
a transient systemd scope whose ``AllowedCPUs`` property creates a cgroup cpuset
bound. Concurrent callers receive disjoint cores, crashed holders are reclaimed,
and a command is launched only after an escape mutation proves that descendants
cannot leave the reserved set. Process affinity alone is deliberately rejected
because a descendant can replace an inherited affinity mask.

Every successful pin reports both the exact core IDs and their count.

CLI::

    cpuset-alloc run --cores K [--tag T] -- CMD [ARGS...]   # pin+run, release on exit
    cpuset-alloc status                                      # held cores + reservations
    cpuset-alloc reclaim                                     # sweep dead holders
    cpuset-alloc selftest [--cores K]                        # mutation self-test (HARD?)
"""

from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path

# Support the `py/bin/cpuset-alloc` symlink (run directly, no install): the package
# parent (py/) may not be on sys.path yet, so fix it up BEFORE the package import
# below. A normal `import`/console-script entry has __package__ set and skips this.
if __name__ == "__main__" and __package__ in (None, ""):  # pragma: no cover
    sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from dagrun import reservation
from dagrun import __version__

PROG = "cpuset-alloc"


def _positive_int(raw: str) -> int:
    try:
        value = int(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid integer: {raw!r}") from exc
    if value < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return value


def _nonnegative_finite_float(raw: str) -> float:
    try:
        value = float(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid number: {raw!r}") from exc
    if not math.isfinite(value) or value < 0:
        raise argparse.ArgumentTypeError("must be finite and >= 0")
    return value


def _wrapped_returncode(returncode: int) -> int:
    """Map a signal-terminated child to the conventional shell status."""
    return 128 - returncode if returncode < 0 else returncode


def _executable_available(command: str) -> bool:
    if os.sep in command:
        return os.path.isfile(command) and os.access(command, os.X_OK)
    return shutil.which(command) is not None


def _cpulist(cores: Sequence[int]) -> str:
    """Render cores as a Linux cpulist (comma form). ``AllowedCPUs=`` and
    ``cpuset.cpus`` both accept this; the kernel may re-render with ranges."""
    return ",".join(str(c) for c in sorted(cores))


def _systemd_run_available() -> bool:
    """True iff ``systemd-run --user --scope`` actually works here (a transient
    user scope can be created). A read of a flag is not evidence — this creates a
    throwaway scope and checks the exit code."""
    if not shutil.which("systemd-run"):
        return False
    try:
        r = subprocess.run(
            ["systemd-run", "--user", "--scope", "--quiet", "--collect", "true"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
            timeout=15,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return r.returncode == 0


def _scope_argv(cores: Sequence[int], cmd: Sequence[str], *, tag: str = "") -> list[str]:
    """Build the ``systemd-run`` argv that boxes ``cmd``'s whole tree onto ``cores``.

    ``--scope`` runs the command SYNCHRONOUSLY in this session and propagates its
    exit code; ``-p AllowedCPUs=`` sets ``cpuset.cpus`` on the scope cgroup so the
    command and every descendant inherit the hard bound; ``--collect`` reaps the
    unit even if the command fails so a leaked scope does not linger."""
    argv = [
        "systemd-run",
        "--user",
        "--scope",
        "--collect",
        "--quiet",
        "-p",
        f"AllowedCPUs={_cpulist(cores)}",
    ]
    if tag:
        # A human-readable unit name aids `systemctl --user list-units` debugging.
        safe = "".join(c if c.isalnum() else "-" for c in tag)[:48]
        argv += ["--unit", f"cpuset-alloc-{safe}-{os.getpid()}"]
    argv.append("--")
    argv.extend(cmd)
    return argv


def _run_reserved_hard(
    cores: Sequence[int], cmd: Sequence[str], *, tag: str = "", prog: str = PROG
) -> int:
    """Run ``cmd`` in a mutation-verified AllowedCPUs scope over reserved cores."""
    if not cmd or not _executable_available(cmd[0]):
        missing = cmd[0] if cmd else "<empty>"
        print(f"{prog}: executable not found or not executable: {missing}", file=sys.stderr)
        return 3
    if not _systemd_run_available():
        print(
            f"{prog}: `systemd-run --user --scope` is unavailable here, so a HARD "
            "cpuset pin cannot be applied. Refusing to run un-pinned (a soft "
            "sched_setaffinity bound is escapable and would silently contaminate the "
            "benchmark). Run on a host with a systemd user session.",
            file=sys.stderr,
        )
        return 3

    probe = _probe_hard_pin(cores)
    if probe.get("verdict") != "HARD":
        print(
            f"{prog}: reserved cores could not be mutation-verified as a HARD "
            f"tree-wide pin; refusing to run: {json.dumps(probe, sort_keys=True)}",
            file=sys.stderr,
        )
        return 3
    assignment = {"cores": list(cores), "count": len(cores)}
    print(
        f"{prog}: reserved {json.dumps(assignment, separators=(',', ':'))}; "
        f"running whole tree pinned via AllowedCPUs={_cpulist(cores)}",
        file=sys.stderr,
    )
    try:
        completed = subprocess.run(
            _scope_argv(cores, cmd, tag=tag), start_new_session=True
        )
    except OSError as exc:
        print(f"{prog}: failed to launch scope ({exc})", file=sys.stderr)
        return 3
    return _wrapped_returncode(completed.returncode)


def cmd_run(args: argparse.Namespace) -> int:
    """Reserve disjoint cores, run one hard-pinned command tree, then release."""
    raw: list[str] = list(args.argv_rest)
    if not raw or raw[0] != "--" or len(raw) == 1:
        print(f"{PROG}: run: no command given (use `-- CMD ARGS...`)", file=sys.stderr)
        return 2
    cmd = raw[1:]
    try:
        with reservation.reserve_cores(
            args.cores,
            tag=args.tag,
            sample_s=args.sample_s,
            max_irq_rate=args.max_irq_rate,
        ) as cores:
            return _run_reserved_hard(cores, cmd, tag=args.tag, prog=f"{PROG}: run")
    except (
        ValueError,
        reservation.InsufficientCoresError,
        reservation.ReservationStateError,
    ) as exc:
        print(f"{PROG}: run: {exc}", file=sys.stderr)
        return 3


def cmd_status(args: argparse.Namespace) -> int:
    """``status``: print the live held cores and every current reservation
    (dead holders swept first, so the answer reflects reality not leaks)."""
    ledger = Path(args.ledger) if args.ledger else None
    try:
        held = reservation.held_cores(ledger=ledger)
    except reservation.ReservationStateError as exc:
        print(f"{PROG}: status: {exc}", file=sys.stderr)
        return 3
    out = {"held_cores": held, "held_count": len(held)}
    print(json.dumps(out, indent=2))
    return 0


def cmd_reclaim(args: argparse.Namespace) -> int:
    """``reclaim``: sweep the ledger and drop records whose holder crashed;
    print the reclaimed records (the leaked-reservation recovery path)."""
    ledger = Path(args.ledger) if args.ledger else None
    try:
        dead = reservation.reclaim_dead(ledger=ledger)
    except reservation.ReservationStateError as exc:
        print(f"{PROG}: reclaim: {exc}", file=sys.stderr)
        return 3
    print(json.dumps({"reclaimed": dead, "reclaimed_count": len(dead)}, indent=2))
    return 0


# --------------------------------------------------------------------------- #
# selftest — VERIFY BY MUTATION, not by reading back what we wrote.            #
# --------------------------------------------------------------------------- #

# Inner script run inside the pinned scope. It directly attempts the forbidden
# sched_setaffinity mutation and deterministically verifies every assigned core.
_SELFTEST_INNER = r"""
import json, os, subprocess, sys, time

excluded = int(sys.argv[1])
assigned = [int(value) for value in sys.argv[2].split(",") if value]

def allowed_list(pid):
    try:
        with open(f"/proc/{pid}/status", "rb") as fh:
            payload = fh.read(65537)
    except OSError:
        return ""
    if len(payload) > 65536:
        return ""
    prefix = b"Cpus_allowed_list:\t"
    values = [line[len(prefix):].strip() for line in payload.split(b"\n")
              if line.startswith(prefix)]
    if len(values) != 1:
        return ""
    try:
        return values[0].decode("ascii")
    except UnicodeError:
        return ""

# --- NEGATIVE: a child cannot be moved to an excluded core ---
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(5)"])
time.sleep(0.2)
before = allowed_list(child.pid)
mutation_attempted = True
mutation_blocked = False
mutation_error = ""
try:
    os.sched_setaffinity(child.pid, {excluded})
except OSError as exc:
    mutation_blocked = True
    mutation_error = str(exc)
after = allowed_list(child.pid)
child.terminate()
child.wait()

# --- POSITIVE: every assigned CPU accepts an exact one-CPU affinity ---
usable = []
for core in assigned:
    try:
        os.sched_setaffinity(0, {core})
        if os.sched_getaffinity(0) == {core}:
            usable.append(core)
    except OSError:
        pass
restore_ok = False
try:
    os.sched_setaffinity(0, set(assigned))
    restore_ok = os.sched_getaffinity(0) == set(assigned)
except OSError:
    pass

print(json.dumps({
    "child_allowed_before": before,
    "child_allowed_after": after,
    "mutation_attempted": mutation_attempted,
    "mutation_blocked": mutation_blocked,
    "mutation_error": mutation_error,
    "positive_cores_usable": sorted(usable),
    "restore_exact": restore_ok,
}))
"""


def _evaluate_probe(
    cores: Sequence[int], excluded: int, inner: dict[str, object]
) -> dict[str, object]:
    """Convert inner mutation evidence into a fail-closed public verdict."""
    attempted = inner.get("mutation_attempted") is True
    blocked = inner.get("mutation_blocked") is True
    unchanged = (
        isinstance(inner.get("child_allowed_before"), str)
        and inner.get("child_allowed_before") == inner.get("child_allowed_after")
    )
    usable_raw = inner.get("positive_cores_usable", [])
    usable = (
        sorted(int(core) for core in usable_raw)
        if isinstance(usable_raw, list)
        else []
    )
    wanted = sorted(int(core) for core in cores)
    positive_ok = usable == wanted and inner.get("restore_exact") is True
    negative_ok = attempted and blocked and unchanged
    verdict = "HARD" if negative_ok and positive_ok else "SOFT_OR_INERT"
    return {
        "verdict": verdict,
        "cores": list(cores),
        "count": len(cores),
        "excluded_core": excluded,
        "mutation_attempted": attempted,
        "mutation_blocked": blocked,
        "negative_escape_masked": negative_ok,
        "positive_cores_usable": usable,
        # Retain the earlier public keys while strengthening their meaning.
        "positive_cores_used": usable,
        "positive_stayed_in_set": positive_ok,
        "inner": inner,
    }


def _probe_hard_pin(cores: Sequence[int]) -> dict[str, object]:
    """Mutation-probe an ``AllowedCPUs`` scope before trusting it.

    Some hosts accept the systemd property yet leave the child able to escape
    onto an excluded core. That is a soft/inert mechanism, not containment.
    """
    allowed = sorted(os.sched_getaffinity(0))
    excluded = next((core for core in allowed if core not in set(cores)), None)
    if excluded is None:
        return {"verdict": "UNTESTABLE", "reason": "no core outside the reserved set"}

    argv = _scope_argv(
        cores,
        [sys.executable, "-c", _SELFTEST_INNER, str(excluded), _cpulist(cores)],
    )
    try:
        completed = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            start_new_session=True,
            timeout=60,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return {"verdict": "ERROR", "reason": str(exc)}

    inner: dict[str, object] = {}
    for line in completed.stdout.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                inner = json.loads(line)
            except json.JSONDecodeError:
                pass
    if not inner:
        return {
            "verdict": "ERROR",
            "reason": "scope probe produced no result",
            "returncode": completed.returncode,
            "stderr": completed.stderr.strip(),
        }

    return _evaluate_probe(cores, excluded, inner)


def cmd_selftest(args: argparse.Namespace) -> int:
    """``selftest``: reserve K cores, hard-pin a scope onto them, and MUTATE —
    try to escape a pinned child onto a K+1th (excluded) core. A HARD bound masks
    the escape; a soft bound lets the child move. Also confirm that every assigned
    core accepts an exact single-core affinity and the full set can be restored. Prints
    a verdict carrying ``{cores, count}`` and exits 0 iff the bound is HARD."""
    if not _systemd_run_available():
        print(
            json.dumps(
                {
                    "verdict": "UNTESTABLE",
                    "reason": "systemd-run --user --scope unavailable",
                }
            )
        )
        return 3

    try:
        with reservation.reserve_cores(
            args.cores,
            tag="selftest",
            sample_s=args.sample_s,
            max_irq_rate=args.max_irq_rate,
        ) as cores:
            result = _probe_hard_pin(cores)
            print(json.dumps(result, indent=2))
            verdict = result.get("verdict")
            if verdict == "HARD":
                return 0
            return 1 if verdict == "SOFT_OR_INERT" else 3
    except (
        ValueError,
        reservation.InsufficientCoresError,
        reservation.ReservationStateError,
    ) as exc:
        print(f"{PROG}: selftest: {exc}", file=sys.stderr)
        return 3


def _build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog=PROG,
        allow_abbrev=False,
        description="Stateful cpuset allocator: reserve DISJOINT cores and HARD-pin "
        "a process tree to them (systemd AllowedCPUs). For benchmark isolation.",
    )
    p.add_argument("--version", action="version", version=f"{PROG} {__version__}")
    sub = p.add_subparsers(dest="command", metavar="<command>")

    run_p = sub.add_parser(
        "run",
        allow_abbrev=False,
        help="reserve K cores and run CMD's whole tree pinned to them",
        description="reserve K cores and run CMD's whole tree pinned to them",
    )
    run_p.add_argument("--cores", type=_positive_int, required=True, metavar="K", help="how many cores to reserve (allocator picks WHICH)")
    run_p.add_argument("--tag", default="", help="label for this reservation (debugging)")
    run_p.add_argument("--sample-s", type=_nonnegative_finite_float, default=0.3, help="/proc/stat idle-sampling window (s)")
    run_p.add_argument("--max-irq-rate", type=_nonnegative_finite_float, default=None, metavar="RATE", help="refuse cores above this measured device-IRQ rate (per second); no default")
    run_p.add_argument("argv_rest", nargs=argparse.REMAINDER, metavar="-- CMD [ARGS...]", help="the command to run pinned")
    run_p.set_defaults(func=cmd_run)

    st_p = sub.add_parser(
        "status", allow_abbrev=False, help="print live held cores + reservations",
        description="print live held cores + reservations",
    )
    st_p.add_argument("--ledger", default="", help="override ledger path (default: $XDG_RUNTIME_DIR)")
    st_p.set_defaults(func=cmd_status)

    rc_p = sub.add_parser(
        "reclaim", allow_abbrev=False, help="sweep dead holders; print reclaimed records",
        description="sweep dead holders; print reclaimed records",
    )
    rc_p.add_argument("--ledger", default="", help="override ledger path")
    rc_p.set_defaults(func=cmd_reclaim)

    stf_p = sub.add_parser(
        "selftest",
        allow_abbrev=False,
        help="mutation self-test: is the pin HARD (inescapable)?",
        description="mutation self-test: is the pin HARD (inescapable)?",
    )
    stf_p.add_argument("--cores", type=_positive_int, default=2, metavar="K", help="cores to reserve for the test (>=2 exercises the positive multi-core check)")
    stf_p.add_argument("--sample-s", type=_nonnegative_finite_float, default=0.3, help="/proc/stat idle-sampling window (s)")
    stf_p.add_argument("--max-irq-rate", type=_nonnegative_finite_float, default=None, metavar="RATE", help="refuse cores above this measured device-IRQ rate (per second); no default")
    stf_p.set_defaults(func=cmd_selftest)

    return p


def main(argv: Sequence[str] | None = None) -> int:
    """Run the CPU-set allocator command and return its process status."""
    parser = _build_parser()
    actual = list(argv) if argv is not None else sys.argv[1:]
    if not actual:
        parser.print_help()
        return 0
    args = parser.parse_args(actual)
    if not hasattr(args, "func"):
        parser.print_help()
        return 0
    if (
        getattr(args, "max_irq_rate", None) is not None
        and getattr(args, "sample_s", 0.0) <= 0
    ):
        parser.error("--sample-s must be > 0 when --max-irq-rate is set")
    func = args.func
    result = func(args)
    return int(result)


if __name__ == "__main__":
    # Support the `py/bin/cpuset-alloc` symlink (run directly, no install): the
    # package parent (py/) may not be on sys.path yet. Mirrors __main__.py.
    _PKG_PARENT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    if _PKG_PARENT not in sys.path:
        sys.path.insert(0, _PKG_PARENT)
    raise SystemExit(main())
