"""Command-line entry point for parallel-experiment-runner.

Subcommands:
  run          run a seed sweep CONTAINED under dagrun's two-level cgroup scope
  plan-round   resolve + print ONE round's enforced width and lowered DAG (dry, no boxing)
  quickstart   print a self-contained getting-started guide

Global:
  --version    print the version and exit
  --userguide  print the full embedded user guide and exit

Every dependency-free invocation (``--help``, ``--version``, no args) exits 0 without importing
any optional dependency: YAML is imported lazily only when a ``.yaml`` spec file is read.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from collections.abc import Mapping, Sequence
from importlib.resources import files
from pathlib import Path

from parallel_experiment_runner import __version__
from parallel_experiment_runner.model import DEFAULT_WALL_TIMEOUT_S

PROG = "parallel-experiment-runner"


def _load_userguide() -> str:
    """The embedded user guide (a real package resource, present in the wheel)."""
    return (files("parallel_experiment_runner") / "USER_GUIDE.md").read_text(encoding="utf-8")


def parse_seeds(spec: str) -> tuple[int, ...]:
    """Parse a seed spec: comma-separated ints and/or inclusive ``lo-hi`` ranges.

    ``"0-4,10,20-22"`` -> ``(0,1,2,3,4,10,20,21,22)``. Order is preserved and duplicates kept
    (a caller may deliberately repeat a seed). Raises ``ValueError`` on malformed input.
    """
    out: list[int] = []
    for part in spec.split(","):
        token = part.strip()
        if not token:
            continue
        if "-" in token.lstrip("-"):
            # a range lo-hi (allow negative lo via the lstrip guard above)
            lo_s, _, hi_s = token.partition("-") if not token.startswith("-") else _split_neg(token)
            lo, hi = int(lo_s), int(hi_s)
            if hi < lo:
                raise ValueError(f"seed range {token!r} has hi < lo")
            out.extend(range(lo, hi + 1))
        else:
            out.append(int(token))
    if not out:
        raise ValueError(f"no seeds parsed from {spec!r}")
    return tuple(out)


def _split_neg(token: str) -> tuple[str, str, str]:
    """Split a range whose low bound is negative, e.g. ``-3-5`` -> (-3, 5)."""
    rest = token[1:]
    lo_rest, _, hi = rest.partition("-")
    return (f"-{lo_rest}", "-", hi)


def _parse_kv(pairs: Sequence[str]) -> dict[str, str]:
    out: dict[str, str] = {}
    for pair in pairs:
        key, sep, value = pair.partition("=")
        if not sep:
            raise ValueError(f"identity field {pair!r} must be key=value")
        out[key.strip()] = value.strip()
    return out


def _spec_from_args(ns: argparse.Namespace) -> "object":
    """Build an ExperimentSpec from a spec file (JSON/YAML) or inline flags."""
    from parallel_experiment_runner.model import ExperimentSpec, HitCondition, WorkerLimits
    from dagrun import parse_size

    if isinstance(ns.spec, str) and ns.spec:
        return _spec_from_file(Path(ns.spec))

    # argparse REMAINDER keeps a leading "--" as the first token; drop it so it is not argv[0].
    command = list(ns.command)
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        raise ValueError("provide a command after `--` (or a --spec file)")
    ns.command = command
    memory = parse_size(ns.memory) if ns.memory else None
    disk = parse_size(ns.disk) if ns.disk else None
    limits = WorkerLimits(
        cpu_cores=int(ns.cpu_cores),
        memory_bytes=memory,
        cpu_timeout_s=int(ns.cpu_timeout) if ns.cpu_timeout is not None else None,
        pids_max=int(ns.pids) if ns.pids is not None else None,
        wall_timeout_s=int(ns.wall_timeout) if ns.wall_timeout is not None else None,
        disk_bytes=disk,
    )
    exit_codes = tuple(int(x) for x in ns.hit_exit_codes)
    if ns.hit_regex or exit_codes:
        hit = HitCondition(regex=ns.hit_regex, hit_exit_codes=exit_codes)
    else:
        hit = HitCondition(hit_exit_codes=(0,))
    identity = _parse_kv(ns.identity)
    return ExperimentSpec(
        name=ns.name,
        command=tuple(ns.command),
        worker_limits=limits,
        hit=hit,
        identity=identity,
        profile_key=ns.profile_key or None,
        detached_resource_report=ns.detached_resource_report,
        detached_unit_prefix=ns.detached_unit_prefix,
    )


def _spec_from_file(path: Path) -> "object":
    """Load an ExperimentSpec from a JSON or YAML file (YAML imported lazily)."""
    from parallel_experiment_runner.model import ExperimentSpec, HitCondition, WorkerLimits

    text = path.read_text(encoding="utf-8")
    if path.suffix in (".yaml", ".yml"):
        try:
            import yaml
        except ModuleNotFoundError as exc:
            raise ValueError(
                f"reading {path} needs PyYAML (pip install pyyaml), or use a .json spec"
            ) from exc
        data = yaml.safe_load(text)
    else:
        data = json.loads(text)
    if not isinstance(data, Mapping):
        raise ValueError(f"{path}: top level must be a mapping")

    wl = data.get("worker_limits", {})
    wl = wl if isinstance(wl, Mapping) else {}
    limits = WorkerLimits(
        cpu_cores=int(wl.get("cpu_cores", 1)),
        memory_bytes=_opt_int(wl.get("memory_bytes")),
        cpu_timeout_s=_opt_int(wl.get("cpu_timeout_s")),
        pids_max=_opt_int(wl.get("pids_max")),
        # Omit or null -> derive the wall backstop at ~3x the CPU budget (see WorkerLimits).
        wall_timeout_s=_opt_int(wl.get("wall_timeout_s")),
        disk_bytes=_opt_int(wl.get("disk_bytes")),
    )
    hd = data.get("hit", {})
    hd = hd if isinstance(hd, Mapping) else {}
    codes = tuple(int(x) for x in hd.get("hit_exit_codes", ()) or ())
    regex = hd.get("regex")
    hit = (
        HitCondition(regex=regex if isinstance(regex, str) else None, hit_exit_codes=codes)
        if (regex or codes)
        else HitCondition(hit_exit_codes=(0,))
    )
    identity = data.get("identity", {})
    identity = {str(k): str(v) for k, v in identity.items()} if isinstance(identity, Mapping) else {}
    env = data.get("env", {})
    env = {str(k): str(v) for k, v in env.items()} if isinstance(env, Mapping) else {}
    command = data.get("command", ())
    if not isinstance(command, Sequence) or isinstance(command, str):
        raise ValueError(f"{path}: 'command' must be a list of argv strings")
    return ExperimentSpec(
        name=str(data.get("name", path.stem)),
        command=tuple(str(x) for x in command),
        worker_limits=limits,
        hit=hit,
        identity=identity,
        profile_key=(str(data["profile_key"]) if data.get("profile_key") else None),
        env=env,
        detached_resource_report=(
            str(data["detached_resource_report"])
            if data.get("detached_resource_report")
            else None
        ),
        detached_unit_prefix=(
            str(data["detached_unit_prefix"])
            if data.get("detached_unit_prefix")
            else None
        ),
    )


def _opt_int(value: object) -> int | None:
    return int(value) if isinstance(value, (int, float)) else None


def _default_slice(work_dir: Path, ns: argparse.Namespace) -> "object":
    """Build the ResourceSlice for this lane: explicit --slice-* flags, else the whole machine."""
    from dagrun import mem_available_bytes, parse_size

    from parallel_experiment_runner.model import ResourceSlice

    cpu = int(ns.slice_cpu) if ns.slice_cpu is not None else (os.cpu_count() or 1)
    mem = (parse_size(ns.slice_memory) or 0) if ns.slice_memory else (mem_available_bytes() or 0)
    if ns.slice_disk:
        disk = parse_size(ns.slice_disk) or 0
    else:
        try:
            st = os.statvfs(str(work_dir))
            disk = st.f_bavail * st.f_frsize
        except OSError:
            disk = 0
    reserve = parse_size(ns.memory_reserve) if ns.memory_reserve else 0
    return ResourceSlice(
        revision=0,
        cpu_cores=cpu,
        memory_bytes=mem,
        disk_bytes=disk,
        lane=ns.lane,
        memory_reserve_bytes=reserve or 0,
    )


def _cmd_run(ns: argparse.Namespace) -> int:
    from parallel_experiment_runner.execute import resolve_cgroup_manager, run_sweep
    from parallel_experiment_runner.profile import ProfileStore

    # Establish boxing FIRST — this may re-exec into a systemd scope and never return.
    manager, code = resolve_cgroup_manager(bool(ns.allow_cgroup_failure))
    if code != 0:
        return code

    try:
        spec = _spec_from_args(ns)
        seeds = parse_seeds(ns.seeds)
    except (ValueError, OSError) as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2

    # Fork-bomb containment (the PID axis) is applied uniformly to every child cgroup at runtime,
    # so it is set on the manager here rather than serialized per-step in the DAG.
    from typing import cast as _cast_spec

    from parallel_experiment_runner.model import ExperimentSpec as _ExperimentSpec

    if manager is not None:
        manager.set_worker_pids_max(_cast_spec(_ExperimentSpec, spec).worker_limits.pids_max)

    work_dir = Path(ns.work_dir).resolve()
    log_dir = Path(ns.log_dir).resolve() if ns.log_dir else work_dir / "ignored" / "logs"
    store_path = Path(ns.profile_store).resolve() if ns.profile_store else (
        work_dir / "ignored" / "parallel-experiment-profile.json"
    )
    ceiling = int(ns.max_concurrency)
    slice_ = _default_slice(work_dir, ns)

    def emit(line: str) -> None:
        """Print one prefixed progress line immediately."""
        print(f"[{PROG}] {line}", flush=True)

    from typing import cast

    from parallel_experiment_runner.model import ExperimentSpec, ResourceSlice

    sweep = run_sweep(
        cast(ExperimentSpec, spec),
        seeds,
        cgroups=manager,
        work_dir=work_dir,
        log_dir=log_dir,
        profile_store=ProfileStore(store_path),
        slice_provider=lambda: cast(ResourceSlice, slice_),
        ceiling=ceiling,
        emit=emit,
    )
    if ns.format == "json":
        print(_sweep_json(sweep))
    # Exit nonzero if any seed hit an infrastructure breach (a kill), so a wrapper notices.
    return 1 if sweep.breaches else 0


def _sweep_json(sweep: "object") -> str:
    from parallel_experiment_runner.execute import SweepResult

    s = sweep if isinstance(sweep, SweepResult) else None
    if s is None:
        return "{}"
    payload = {
        "spec": s.spec_name,
        "profile_key": s.profile_key,
        "ephemeral": s.ephemeral,
        "estimate": {
            "wall_s": s.up_front.wall_s,
            "cpu_s": s.up_front.cpu_s,
            "peak_mem_bytes": s.up_front.peak_mem_bytes,
            "peak_tasks": s.up_front.peak_tasks,
            "samples": s.up_front.samples,
            "source": s.up_front.source,
        },
        "total_wall_s": s.total_wall_s,
        "total_cpu_s": s.total_cpu_s,
        "throughput_seeds_per_s": s.throughput_seeds_per_s,
        "hits": list(s.hits),
        "rounds": [
            {
                "width": r.width,
                "wall_s": r.wall_s,
                "cpu_s": r.cpu_s,
                "limiting_dimension": r.limiting_dimension,
                "reaped_leftovers": r.reaped_leftovers,
                "outcomes": [
                    {
                        "seed": o.seed,
                        "status": o.status,
                        "returncode": o.returncode,
                        "wall_s": o.wall_s,
                        "cpu_s": o.cpu_s,
                        "peak_bytes": o.peak_bytes,
                        "tasks_peak": o.tasks_peak,
                        "breach": o.breach,
                        "log_path": o.log_path,
                        "resource_report_error": o.resource_report_error,
                    }
                    for o in r.outcomes
                ],
            }
            for r in s.rounds
        ],
    }
    return json.dumps(payload, indent=2, sort_keys=True)


def _cmd_plan_round(ns: argparse.Namespace) -> int:
    """Resolve ONE round's enforced width and print the lowered DAG — dry, no boxing, for
    inspection and tests. Uses the DECLARED per-worker caps as the per-instance footprint."""
    from dagrun import to_ascii

    from parallel_experiment_runner.calibrate import (
        PerInstance,
        live_capacity,
        resolve_width,
    )
    from parallel_experiment_runner.model import CostEstimate
    from parallel_experiment_runner.planner import RoundPlan, generate_round_dag
    from parallel_experiment_runner.profile import profile_identity

    try:
        spec = _spec_from_args(ns)
        seeds = parse_seeds(ns.seeds)
    except (ValueError, OSError) as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2

    from typing import cast

    from parallel_experiment_runner.model import ExperimentSpec

    spec_t = cast(ExperimentSpec, spec)
    work_dir = Path(ns.work_dir).resolve()
    slice_ = _default_slice(work_dir, ns)
    live = live_capacity(work_dir)
    limits = spec_t.worker_limits
    per_inst = PerInstance(
        cpu_cores=limits.cpu_cores, memory_bytes=limits.memory_bytes, disk_bytes=limits.disk_bytes
    )
    from typing import cast as _cast

    from parallel_experiment_runner.model import ResourceSlice

    fit = resolve_width(_cast(ResourceSlice, slice_), live, per_inst, int(ns.max_concurrency))
    width = max(1, fit.width) if fit.width >= 1 else 0
    identity = profile_identity(spec_t)
    print(f"profile key: {identity.key}{' (ephemeral)' if identity.ephemeral else ''}")
    print(
        f"resolved width: {fit.width} (limiting={fit.limiting_dimension}; "
        f"cpu_slots={fit.cpu_slots}, mem_slots={fit.mem_slots}, disk_slots={fit.disk_slots}, "
        f"ceiling={fit.ceiling})"
    )
    if width < 1:
        print("box too small for even one worker under the current lane/live capacity.")
        return 0
    first_batch = tuple(seeds[:width])
    plan = RoundPlan(
        spec=spec_t,
        seeds=first_batch,
        width=width,
        slice_revision=0,
        limiting_dimension=fit.limiting_dimension,
        log_dir=work_dir / "ignored" / "logs",
        per_worker_estimate=CostEstimate.unset(),
    )
    dag = generate_round_dag(plan)
    print(f"first round: {len(first_batch)} seed(s) at width {width}")
    print(to_ascii(dag))
    return 0


def build_parser() -> argparse.ArgumentParser:
    """Build the complete command-line parser."""
    parser = argparse.ArgumentParser(
        prog=PROG,
        description=(
            "Run N concurrent seed-sweep workers under dagrun's cgroup RESOURCE "
            "CONTAINMENT (CPU-time / memory / PID / wall caps)."
        ),
    )
    parser.add_argument("--version", action="version", version=f"{PROG} {__version__}")
    parser.add_argument(
        "--userguide",
        action="store_true",
        help="print the full embedded user guide (the complete reference) and exit",
    )
    sub = parser.add_subparsers(dest="subcommand", metavar="<command>")

    for name, help_text in (
        ("run", "run a seed sweep under cgroup resource containment"),
        ("plan-round", "resolve + print one round's width and DAG (dry, no containment)"),
    ):
        sp = sub.add_parser(name, help=help_text, description=help_text)
        sp.add_argument("--name", default="sweep", help="experiment name (default: sweep)")
        sp.add_argument("--spec", default=None, help="JSON/YAML spec file (overrides inline flags)")
        sp.add_argument("--seeds", default="0-9", help="seed spec, e.g. '0-99,200,300-305'")
        sp.add_argument("--cpu-cores", type=int, default=1, help="per-worker CPU cores (cpu.max)")
        sp.add_argument("--memory", default=None, help="per-worker memory cap, e.g. 4G (memory.max)")
        sp.add_argument(
            "--cpu-timeout",
            type=int,
            default=None,
            help="per-worker CPU-SECOND budget (user+sys). Omit = UNSET (never wall-derived)",
        )
        sp.add_argument(
            "--pids",
            "--max-pids",
            type=int,
            default=None,
            dest="pids",
            help="per-worker PID cap (pids.max) — the fork-bomb guard. Omit = no cap",
        )
        sp.add_argument(
            "--wall-timeout",
            type=int,
            default=None,
            help=(
                "per-worker wall backstop seconds (defence-in-depth). Omit = DERIVE: ~3x the "
                f"--cpu-timeout budget when set, else {DEFAULT_WALL_TIMEOUT_S}"
            ),
        )
        sp.add_argument("--disk", default=None, help="per-worker disk reserve, e.g. 8G")
        sp.add_argument(
            "--hit-regex", default=None, help="regex over a worker's log that marks a HIT"
        )
        sp.add_argument(
            "--hit-exit-codes",
            nargs="*",
            default=(),
            metavar="CODE",
            help="exit codes that mark a HIT (default: 0 when no regex/codes given)",
        )
        sp.add_argument(
            "--identity",
            nargs="*",
            default=(),
            metavar="K=V",
            help="apples-to-apples identity fields hashed into the profile key",
        )
        sp.add_argument("--profile-key", default=None, help="manual profile key label")
        sp.add_argument(
            "--max-concurrency",
            type=int,
            default=64,
            help=(
                "requested ceiling on concurrent workers (default 64; machine hard cap 316)"
            ),
        )
        sp.add_argument("--slice-cpu", type=int, default=None, help="lane CPU cores (default: all)")
        sp.add_argument("--slice-memory", default=None, help="lane memory, e.g. 64G (default: avail)")
        sp.add_argument(
            "--memory-reserve",
            default="0",
            help=(
                "memory kept outside the lane and subtracted from live MemAvailable every round "
                "(default: 0; e.g. 64G)"
            ),
        )
        sp.add_argument("--slice-disk", default=None, help="lane disk, e.g. 200G (default: free)")
        sp.add_argument("--lane", default="", help="lane label (informational)")
        sp.add_argument("--work-dir", default=".", help="base dir for logs + profile store")
        sp.add_argument("--log-dir", default=None, help="per-worker log dir (default: <wd>/ignored/logs)")
        sp.add_argument("--profile-store", default=None, help="profile-store JSON path")
        sp.add_argument(
            "--detached-resource-report",
            default=None,
            help=(
                "per-seed append-friendly key=value report for a child unit outside the worker "
                "cgroup; must contain {seed}"
            ),
        )
        sp.add_argument(
            "--detached-unit-prefix",
            default=None,
            help=(
                "required unit-name prefix for detached report accounting and cleanup; must be "
                "set with --detached-resource-report"
            ),
        )
        sp.add_argument("--format", choices=["human", "json"], default="human", help="output format")
        if name == "run":
            sp.add_argument(
                "--allow-cgroup-failure",
                action="store_true",
                help="run UNCONTAINED if cgroup containment can't be established (NOT recommended)",
            )
        sp.add_argument(
            "command",
            nargs=argparse.REMAINDER,
            help="the workload argv after `--`, using {seed} where the seed goes",
        )

    sub.add_parser("quickstart", help="print a self-contained getting-started guide", description="print a self-contained getting-started guide")
    return parser


_QUICKSTART = f"""\
{PROG} — run N concurrent seed-sweep workers under dagrun's RESOURCE CONTAINMENT.

Why: unbounded parallel experiments once saturated the host to ~470 concurrent processes,
starving the very measurements they existed to produce. This is a resource box, not a
security sandbox — it defends against a BUG in our own code (leak memory, run forever, fork
bomb), NOT against an adversary, so it never reaches for seccomp or user-namespace isolation.
It makes concurrency a DECLARED, ENFORCED number and runs every worker under four cgroup
axes, each mapped to one failure mode, with a clean kill that NAMES what breached:

  * cpu    — "run forever": a CPU-SECOND budget (--cpu-timeout), the load-immune guard.
  * memory — "leak memory": an inner memory.max cap (--memory).
  * pids   — "fork bomb":   an inner pids.max cap (--pids). A fork bomb exhausts PIDs, which
             neither the cpu nor the memory cap can contain — this is a distinct axis.
  * wall   — defence-in-depth hang backstop only (--wall-timeout); derived at ~3x the CPU
             budget when left unset.

Example — sweep seeds 0..199, 1 core + 4G + a 120 CPU-second budget + a 512-PID cap per
worker, hunting a divergence in the log, capped at 32 concurrent workers:

  {PROG} run \\
    --name chaos-divergence --seeds 0-199 \\
    --cpu-cores 1 --memory 4G --cpu-timeout 120 --pids 512 \\
    --hit-regex 'DIVERGENCE|panic' --max-concurrency 32 \\
    --identity backend=ptrace image=demo5 \\
    -- ./workload --chaos --seed {{seed}}

Notes:
  * CPU-time is the real, load-immune guard; omit --cpu-timeout to leave it UNSET (honest).
    Never derive a CPU budget from wall time — on an N-core host it would be ~1/N too tight.
  * --wall-timeout is only a backstop; omit it to derive ~3x the CPU budget automatically.
  * The runner ramps width 1 -> 2 -> 4 -> …, measuring per-worker footprint before scaling.
  * Per-worker logs go to <work-dir>/ignored/logs/seed-<n>.log; the run prints only a summary.
  * `plan-round` shows the resolved width + DAG without running (and needs no cgroups).
"""


def main(argv: Sequence[str] | None = None) -> int:
    """Run the command-line interface and return its process exit code."""
    parser = build_parser()
    ns = parser.parse_args(list(argv) if argv is not None else None)

    if bool(getattr(ns, "userguide", False)):
        sys.stdout.write(_load_userguide())
        return 0

    name = ns.subcommand if isinstance(getattr(ns, "subcommand", None), str) else None
    if name is None:
        parser.print_help()
        return 0
    if name == "quickstart":
        print(_QUICKSTART)
        return 0
    if name == "run":
        return _cmd_run(ns)
    if name == "plan-round":
        return _cmd_plan_round(ns)
    parser.print_help()
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
