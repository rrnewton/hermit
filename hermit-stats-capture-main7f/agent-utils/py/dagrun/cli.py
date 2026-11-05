"""Command-line interface for dagrun.

Subcommands:
  run --dag FILE    run a DAG under two-level cgroup-v2 boxing (exit 0 iff every step passes;
                    boxing is ON by default — pass --allow-cgroup-failure to run un-boxed)
  list --dag FILE   list the steps
  ascii --dag FILE  draw the DAG as ASCII art
  dot --dag FILE    emit Graphviz DOT (pipe to `dot -Tsvg`)
  json --dag FILE   re-emit the DAG as canonical JSON
  yaml --dag FILE   re-emit the DAG as YAML
  quickstart        print a self-contained getting-started guide
  --userguide       print the full embedded user guide (the complete reference)

A DAG is a JSON or YAML file (see `quickstart` for the schema + a runnable example). `--dag`
auto-detects the format by extension: `.yaml`/`.yml` load as YAML (isomorphic to the JSON schema,
and additionally allow comments + multi-line block-scalar descriptions), everything else as JSON.
Pass `--dag -` to read a JSON DAG from stdin.
"""

from __future__ import annotations

import argparse
import dataclasses
import math
import os
import resource
import shlex
import statistics
import subprocess
import sys
import time
from collections.abc import Mapping, Sequence
from importlib.resources import files
from pathlib import Path

from dagrun import __version__
from dagrun import admission
from dagrun.capabilities import enforcement_manifest
from dagrun import summary as summarylib
from dagrun import sync as synclib
from dagrun.estimates import (
    Plan,
    Planner,
    Sample,
    apply_plan_to_config,
    build_plan,
    feedback_identity,
    load_step_samples,
    load_step_speedups,
    plan_to_json,
    plan_to_text,
    step_speedups_from_buckets,
    write_scaling_model,
)
from dagrun.memory_feedback import (
    MemoryAdmission,
    apply_memory_admissions,
    load_memory_admissions,
)
from dagrun.summary import DEFAULT_RESERVOIR_K, Summary
from dagrun.io import (
    DagJsonError,
    dag_from_json,
    dag_from_path,
    dag_to_json,
    dag_to_yaml,
)
from dagrun.model import (
    CPU_TIMEOUT_MULTIPLIER_ENV,
    CPU_TIMEOUT_PLATFORM_ENV,
    DEFAULT_CPU_TIMEOUT_MULTIPLIER,
    DEFAULT_SMALL_CPU_COUNT,
    DEFAULT_SMALL_CPU_TIMEOUT,
    DEFAULT_SMALL_MEM_CAP_BYTES,
    DEFAULT_STEP_TIMEOUT,
    JOBS_ENV_ENV,
    CmdType,
    DagConfig,
    ResourceHint,
    Step,
    cmdtype_env_with_inner_jobs,
    command_with_inner_jobs,
    effective_cpu_count,
    env_with_inner_jobs,
    resolve_cpu_timeout_multiplier,
    step_classification,
    step_width_is_resizable,
)
from dagrun.profile_capture import (
    CaptureConfig,
    IsolatedTrialRequest,
    IsolatedTrialResult,
    ProfileCaptureError,
    SweetSpotSelection,
    capture_at_sweet_spot,
    select_sweet_spot,
)
from dagrun.profile_enrich import container_core_budget
from dagrun.protocols import CgroupManager, MetricsSink, RunWindow, StepOutcome
from dagrun.reservation import Reservation
from dagrun.scheduler import (
    _checked_jobs_env_export,
    MAX_PROFILE_TIMESERIES_INTERVAL_S,
    MIN_PROFILE_TIMESERIES_INTERVAL_S,
    _self_managed_width_error,
    cap_config_max_cpus,
    nested_run_refusal,
    run_dag_limited,
)
from dagrun.sizing import (
    _step_mem_cap_for_inner_jobs,
    jobs_for_budget,
    parse_size,
    transitive_deps,
)
from dagrun.sweep import (
    CpuTopology,
    detect_cpu_topology,
    labeled_width_grid_for_pass,
    limit_topology,
    parse_target_duration,
    parse_widths,
    stable_topological_steps,
    workload_digest,
    width_grid_for_pass,
)
from dagrun.viz import GROUP_BY_KEYS, to_ascii, to_ascii_groups, to_dot, to_dot_groups

PROG = "dagrun"
CGROUP_SETUP_ENVIRONMENT_ERROR = (
    "ENVIRONMENT: managed cgroup scope could not quiesce and delegate per-step controllers; "
    "no DAG node started and no product build started"
)

#: Environment variable overriding the default profile-store location (Feature D). An explicit
#: ``--perf-dir`` still wins over this; ``--no-profile`` disables logging entirely.
PROFILE_DIR_ENV = "DAGRUN_PROFILE_DIR"

#: Default profile-store directory, RELATIVE TO THE CURRENT WORKING DIRECTORY, used when neither
#: ``--perf-dir`` nor ``$DAGRUN_PROFILE_DIR`` is set and ``--no-profile`` is absent. Created on
#: demand. Runs (and sweeps) auto-append here so profiling data lands somewhere obvious and
#: browsable without any flag.
DEFAULT_PROFILE_DIR = os.path.join(".dagrun", "profiles")

# Largest total-CPU limit safe to encode in the fixed 100000-us per-step cpu.max period.
MAX_RUN_CPUS = (2**63 - 1) // 100_000
# Compatibility name retained for callers that imported the 0.13 module constant directly.
MAX_RUN_CPU_JOBS = MAX_RUN_CPUS
# Hard bound on config objects materialized by --stress before any graph expansion occurs.
MAX_STRESS_GENERATED_NODES = 100_000


def _parse_profile_timeseries_duration(raw: str) -> float:
    """Parse the opt-in sampling interval, bounded to a useful and safe range."""

    try:
        seconds = parse_target_duration(raw)
    except ValueError:
        raise ValueError(
            f"invalid --profile-timeseries {raw!r}: expected seconds or an ms/s/m/h suffix"
        ) from None
    if not (
        MIN_PROFILE_TIMESERIES_INTERVAL_S
        <= seconds
        <= MAX_PROFILE_TIMESERIES_INTERVAL_S
    ):
        raise ValueError(
            f"invalid --profile-timeseries {raw!r}: duration must be between "
            f"{MIN_PROFILE_TIMESERIES_INTERVAL_S * 1000:g}ms and "
            f"{MAX_PROFILE_TIMESERIES_INTERVAL_S:g}s"
        )
    return seconds


# --------------------------------------------------------------------------- colors
class Palette:
    """Minimal ANSI colorizer (stdlib only). Disabled for non-ttys and when NO_COLOR is set."""

    def __init__(self, enabled: bool) -> None:
        self.enabled = enabled

    def _wrap(self, code: str, text: str) -> str:
        return f"\033[{code}m{text}\033[0m" if self.enabled else text

    def bold(self, text: str) -> str:
        """Render ``text`` with bold emphasis when colors are enabled."""
        return self._wrap("1", text)

    def dim(self, text: str) -> str:
        """Render ``text`` with dim emphasis when colors are enabled."""
        return self._wrap("2", text)

    def green(self, text: str) -> str:
        """Render ``text`` in green when colors are enabled."""
        return self._wrap("32", text)

    def red(self, text: str) -> str:
        """Render ``text`` in bold red when colors are enabled."""
        return self._wrap("1;31", text)

    def yellow(self, text: str) -> str:
        """Render ``text`` in yellow when colors are enabled."""
        return self._wrap("33", text)

    def cyan(self, text: str) -> str:
        """Render ``text`` in cyan when colors are enabled."""
        return self._wrap("36", text)


def _color_enabled(stream: object) -> bool:
    if os.environ.get("NO_COLOR"):
        return False
    isatty = getattr(stream, "isatty", None)
    return bool(callable(isatty) and isatty())


def _banner(c: Palette) -> str:
    return (
        f"{c.bold(PROG)} {c.dim('v' + __version__)}\n"
        "Run a DAG of build/test steps concurrently and safely: two-level cgroup CPU/memory\n"
        "boxing with zombie-free teardown, memory-aware concurrency, always-on resource\n"
        "logging, and DAG visualization."
    )


def _epilog(c: Palette) -> str:
    ex = c.cyan
    return (
        f"{c.bold('examples')}\n"
        f"  {ex(f'{PROG} quickstart')}                       {c.dim('# get started (schema + runnable demo)')}\n"
        f"  {ex(f'{PROG} run --dag dag.json')}               {c.dim('# run it; exit 0 iff all steps pass')}\n"
        f"  {ex(f'{PROG} run --dag dag.json --profile')}     {c.dim('# ...and print a per-step profile table')}\n"
        f"  {ex(f'{PROG} run --dag dag.json --selected build.app')} {c.dim('# run one selected step and its dependencies')}\n"
        f"  {ex(f'{PROG} run --dag dag.json --selected test.unit --stress 10 -s 2 -j 100')} {c.dim('# 2 active graph copies sharing 100 CPU-equivalents')}\n"
        f"  {ex(f'{PROG} plan --dag dag.json --planner critical-path')} {c.dim('# show learned estimates + the plan')}\n"
        f"  {ex(f'{PROG} sweep --dag dag.json --step build.app --jobs 1..8')} {c.dim('# parallel-speedup study')}\n"
        f"  {ex(f'{PROG} ascii --dag dag.json')}             {c.dim('# quick ASCII view of the graph')}\n"
        f"  {ex(f'{PROG} dot --dag dag.json | dot -Tsvg -o dag.svg')}\n\n"
        f"{c.dim('Profiling data auto-logs to ./.dagrun/profiles/ by default')}\n"
        f"{c.dim(f'(sweep: --output-dir, with --perf-dir as an alias; other commands: --perf-dir; ${PROFILE_DIR_ENV} also overrides; --no-profile disables).')}"
    )


# --------------------------------------------------------------------------- quickstart
def _quickstart(c: Palette) -> str:
    h = c.bold
    k = c.cyan
    return f"""{_banner(c)}

{h('1. Install')}
  pip install dagrun

{h('2. Write a DAG (JSON)')}  {c.dim('- a list of steps; each depends on others by "group.job" tag')}
  Save as dag.json:
  {{
    "resource_caps": {{"browser": 1}},
    "steps": [
      {{"group": "build", "job": "app", "desc": "compile", "cmd": "echo build && sleep 0.2"}},
      {{"group": "test",  "job": "unit", "desc": "unit tests", "cmd": "echo test && sleep 0.2",
        "deps": ["build.app"]}},
      {{"group": "e2e",   "job": "smoke", "desc": "browser smoke", "cmd": "echo e2e && sleep 0.2",
        "deps": ["build.app"], "hint": {{"resources": {{"browser": 1}}}}}}
    ]
  }}

{h('3. Look at it, then run it')}
  {k(f'{PROG} list  --dag dag.json')}
  {k(f'{PROG} ascii --dag dag.json')}
  {k(f'{PROG} run   --dag dag.json')}        {c.dim('# concurrent, dependency-ordered; exit 0 = all passed')}

{h('Profile & experiment with individual steps')}
  {k(f'{PROG} run   --dag dag.json --profile')}          {c.dim('# print a per-step timing/memory table after the run')}
  {k(f'{PROG} run   --dag dag.json --selected test.unit')}   {c.dim('# run that step and all of its dependencies')}
  {k(f'{PROG} run   --dag dag.json --selected build.app,test.unit')}  {c.dim('# one or more, comma-separated')}
  {k(f'{PROG} sweep --dag dag.json --step build.app --jobs 1..8')}  {c.dim('# run one step at -j1..-j8, print a speedup table')}
  {c.dim('--selected includes every dependency needed by the named steps. Add')}
  {c.dim('--ignore-selected-deps only when those inputs are already present and must not run.')}
  {c.dim('sweep passes each width through cmdtype, jobs_flag, and/or jobs_env and reports wall/user/sys/rss + speedup.')}

{h('Where profiling data lands (by default)')}
  {c.dim('Every run and sweep AUTO-LOGS resource-usage CSVs to a repo-local store:')}
    {k('./.dagrun/profiles/')}   {c.dim('(created on demand, relative to CWD)')}
  {c.dim(f'Override with {k("--perf-dir DIR")} or ${PROFILE_DIR_ENV}; turn it off with {k("--no-profile")}.')}
  {c.dim('The tool prints exactly where it appended, so the logging is never silent. Consider')}
  {c.dim('gitignoring ./.dagrun/ (or check it in to keep a history - project choice).')}

{h('Smarter planning: learned estimates + the critical-path planner')}
  {c.dim('The runner FEEDS the recorded store back in at plan time: for each step it derives a')}
  {c.dim('robust est_duration (contention-discounted MEDIAN of past elapsed_s) and an rss estimate')}
  {c.dim('(a high percentile of past peak_bytes), and USES them in place of the DAG-authored hint')}
  {c.dim('when the store has enough samples. So est_duration_s no longer has to be hand-authored -')}
  {c.dim('planning improves automatically as runs accumulate. This also feeds --max-mem sizing.')}
  {k(f'{PROG} plan  --dag dag.json')}                       {c.dim('# show the plan (est + source, rss, bottom-level, order)')}
  {k(f'{PROG} plan  --dag dag.json --planner critical-path')} {c.dim('# order by longest remaining path, not just single est')}
  {k(f'{PROG} plan  --dag dag.json --format json')}          {c.dim('# canonical machine-readable plan (for a CI-optimizing agent)')}
  {k(f'{PROG} run   --dag dag.json --planner critical-path --show-plan')} {c.dim('# print the plan, then run in that order')}
  {c.dim('--planner greedy-lpt (default) launches the longest single step first; critical-path')}
  {c.dim('launches the step with the highest bottom-level (longest remaining est-weighted path).')}
  {c.dim('When the store holds multiple inner-jobs widths for a step, plan/--show-plan also print a')}
  {c.dim('parallel-speedup section: the recommended inner_jobs (best wall before the diminishing-')}
  {c.dim('returns knee + within the core budget), achieved effective_cores, and the speedup curve.')}
  {c.dim(f'Use {k("--no-profile-feedback")} to ignore the store and plan from the DAG hints only.')}

{h('DAG schema')}  {c.dim('(only group/job/cmd are required per step; everything else has defaults)')}
  step:   group, job, desc, description, labels[], cmd, cmdtype, manifest{{lane,category,test?,mode?,backend?}}, result_manifests[], deps[], env{{}}, timeout, jobs_flag, jobs_env, networkonly, engine_only, delegated_children, hint{{}}
  hint:   resources{{name:int}}, est_duration_s, rss_baseline_bytes, hard_mem_max_bytes,
          classification("cpu-bound"|"latency-bound"|"light"), preferred_inner_jobs
  top:    include[{{path,namespace,after[]?}}], description, resource_caps{{name:int}}, mem_cap_factor, mem_cap_floor_bytes,
          outer_mem_safety_factor, default_step_timeout, default_jobs_flag, default_jobs_env
  {c.dim('desc = short label; description = long-form docs (often multi-line, great in YAML).')}
  {c.dim('YAML: --dag also accepts .yaml/.yml (isomorphic to JSON; allows comments + block-scalar')}
  {c.dim('  descriptions). The yaml subcommand emits YAML; json emits canonical JSON.')}
  {c.dim('include: load-time namespaced file composition; optional after[] wires outer prerequisites.')}
  {c.dim('delegated_children: opt into child dagrun; preserves cgroups or names an unboxed fallback.')}
  {c.dim('resource_caps bound concurrent demand - e.g. {"browser":1} serializes browser steps.')}
  {c.dim('cmdtype: unknown (default) | make | cargo-build | cargo-test | cargo-nextest |')}
  {c.dim('  generic-dash-j-command | generic-with-flag. Known types append width arguments,')}
  {c.dim('  or set DAGRUN_EXTRA_ARGS for an unquoted placement in a compound command.')}
  {c.dim('jobs_flag: template appended with a step preferred_inner_jobs, e.g. "-j" -> "-j 4",')}
  {c.dim('  "-j%d" -> "-j4", "--jobs=" -> "--jobs=4", "--num-threads" -> "--num-threads 4".')}
  {c.dim('  Empty/whitespace means a fixed self-managed width: it cannot be rewritten or swept.')}
  {c.dim('jobs_env: environment variable receiving the same admitted width; None inherits')}
  {c.dim(f'  default_jobs_env, normally resolved from ${JOBS_ENV_ENV}.')}

{h('What you get')}
  - concurrent scheduling honoring deps + resource caps, ordered by the chosen {k('--planner')}
    {c.dim('(greedy-lpt = longest single step first; critical-path = longest remaining path first)')}
    {c.dim('(--max-steps bounds active DAG steps; --max-cpus caps each width + outer bandwidth)')}
  - learned est_duration / rss from the profile store override the DAG hints at plan time
    {c.dim('(disable with --no-profile-feedback; inspect with the plan subcommand / --show-plan)')}
  - a failing step fails the run (exit 1) and, by default, eager-cancels in-flight steps
    ({k('--keep-going')} continues launching independent ready steps; failed dependents are skipped)
  - Linux cgroup-v2 per-step memory/CPU boxing is ON BY DEFAULT (the tool's primary purpose):
    {k('run')} re-execs inside a systemd --user scope and caps each step in its own child cgroup
    {c.dim('(no cgroup-v2 + systemd --user scope? the run errors — pass')} {k('--allow-cgroup-failure')} {c.dim('to run un-boxed)')}
  - {k('run --max-mem 8G')} derives a conservative model-based active-step ceiling from RAM hints
    {c.dim('(with explicit --max-steps, the tighter ceiling wins)')}
  - {k('run --perf-dir DIR')} writes per-step + whole-run resource-usage CSVs into DIR

{h('Python API')}  {c.dim('(same engine, in code)')}
  from dagrun import Step, ResourceHint, DagConfig, run_dag_limited, to_ascii
  cfg = DagConfig(steps=(Step("build","app","compile","echo build && sleep 0.1"),))
  print(to_ascii(cfg)); result = run_dag_limited(cfg, max_steps=2, max_cpus=8)

{h('Exit codes')}  0 = all steps passed | 1 = a step failed | 2 = bad usage / bad DAG file
             | 3 = cgroup boxing required but unavailable (use {k('--allow-cgroup-failure')})
"""


# --------------------------------------------------------------------------- user guide
def _load_userguide() -> str:
    """Return the full user guide, read from the guide EMBEDDED IN THIS PACKAGE.

    The guide is a generated package resource (``dagrun/USER_GUIDE.md``, declared as
    ``package-data``). Reading it via ``importlib.resources`` makes ``--userguide`` work from an
    installed wheel without relying on a source checkout."""
    return (files("dagrun") / "USER_GUIDE.md").read_text(encoding="utf-8")


# --------------------------------------------------------------------------- rendering
def _render_list(cfg: DagConfig, c: Palette) -> str:
    if not cfg.steps:
        return "(empty DAG)"
    width = max(len(s.tag) for s in cfg.steps)
    lines: list[str] = []
    for step in cfg.steps:
        tag = c.bold(f"{step.tag:<{width}}")  # pad first, then color (keeps alignment)
        cls = c.yellow(f"[{step_classification(step).value}]")
        dep = c.dim("  <- " + ", ".join(step.deps)) if step.deps else ""
        lines.append(f"{tag}  {cls} {step.desc}{dep}")
    return "\n".join(lines)


# --------------------------------------------------------------------------- parser
class _ColorHelp(argparse.RawDescriptionHelpFormatter):
    """Raw formatter so our colored description/epilog render verbatim."""


def _positive_int(raw: str) -> int:
    try:
        value = int(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid integer: {raw!r}") from exc
    if value < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return value


def _positive_i64(raw: str) -> int:
    digits = raw[1:] if raw[:1] in {"+", "-"} else raw
    if not digits or not digits.isascii() or not digits.isdigit():
        raise argparse.ArgumentTypeError(f"invalid integer: {raw!r}")
    value = _positive_int(raw)
    if value > 2**63 - 1:
        raise argparse.ArgumentTypeError("must be <= 9223372036854775807")
    return value


def _run_max_cpus(raw: str) -> int:
    value = _positive_i64(raw)
    if value > MAX_RUN_CPUS:
        raise argparse.ArgumentTypeError(f"must be <= {MAX_RUN_CPUS}")
    return value


class _MaxCpusAction(argparse.Action):
    """Merge canonical and compatibility spellings while rejecting disagreement."""

    def __call__(
        self,
        parser: argparse.ArgumentParser,
        namespace: argparse.Namespace,
        values: object,
        option_string: str | None = None,
    ) -> None:
        if not isinstance(values, int):
            parser.error(f"{option_string or '--max-cpus'} requires an integer")
        source = option_string or "--max-cpus"
        existing = getattr(namespace, self.dest, None)
        if isinstance(existing, int) and existing != values:
            existing_source = getattr(
                namespace, "_max_cpus_source", "an earlier CPU limit"
            )
            if source == "--jobs" or existing_source == "--jobs":
                parser.error("--max-cpus and legacy --jobs disagree")
            parser.error(
                f"{source}: conflicts with {existing_source} ({values} != {existing})"
            )
        setattr(namespace, self.dest, values)
        if not hasattr(namespace, "_max_cpus_source"):
            setattr(namespace, "_max_cpus_source", source)


def build_parser() -> argparse.ArgumentParser:
    """Build the complete command-line argument parser."""
    c = Palette(_color_enabled(sys.stdout))
    parser = argparse.ArgumentParser(
        prog=PROG,
        allow_abbrev=False,
        formatter_class=_ColorHelp,
        description=_banner(c),
        epilog=_epilog(c),
    )
    parser.add_argument("--version", action="version", version=f"{PROG} {__version__}")
    parser.add_argument(
        "--userguide",
        action="store_true",
        help="print the full embedded user guide (the complete reference) and exit",
    )
    sub = parser.add_subparsers(dest="command", metavar="<command>")

    run_p = sub.add_parser(
        "run", allow_abbrev=False, help="run a DAG (exit 0 iff every step passes)",
        description="run a DAG (exit 0 iff every step passes)",
        formatter_class=_ColorHelp,
        epilog="DAG field cmdtype valid values:\n"
        "  unknown (default)\n"
        "  make\n"
        "  cargo-build\n"
        "  cargo-test\n"
        "  cargo-nextest\n"
        "  generic-dash-j-command\n"
        "  generic-with-flag\n"
        "Known types append their width arguments unless cmd contains unquoted "
        "$DAGRUN_EXTRA_ARGS or ${DAGRUN_EXTRA_ARGS}.",
    )
    run_p.add_argument(
        "--dag",
        required=True,
        metavar="FILE",
        help=(
            "DAG file; .yaml/.yml load as YAML, else JSON; files may use namespaced "
            "include fragments ('-' = self-contained JSON stdin only)"
        ),
    )
    run_p.add_argument(
        "-s",
        "--max-steps",
        type=_positive_i64,
        default=None,
        metavar="N",
        help="maximum active DAG steps (default: effective --max-cpus limit)",
    )
    run_p.add_argument(
        "-j",
        "--max-cpus",
        dest="max_cpus",
        action=_MaxCpusAction,
        type=_run_max_cpus,
        default=None,
        metavar="N",
        help="outer CPU-bandwidth limit and maximum width of any one runner-controlled step "
        "(default: effective container/affinity budget tightened by the shared 90%% slice; "
        "a nested run is clamped to its delegated parent's effective CPU envelope)",
    )
    run_p.add_argument(
        "--jobs",
        dest="max_cpus",
        action=_MaxCpusAction,
        type=_run_max_cpus,
        default=None,
        metavar="N",
        help=argparse.SUPPRESS,
    )
    run_p.add_argument(
        "--cores",
        "--cpuset",
        "--pin",
        type=_positive_int,
        default=None,
        metavar="K",
        help="CPU PINNING (cpuset/affinity), OPT-IN and OFF BY DEFAULT: constrain the WHOLE run "
        "process tree (all steps + descendants) to K least-busy FREE cores. Intended for "
        "controlled measurements, not ordinary CI. Never pins a fixed core id. Requires an "
        "exact, hard cgroup cpuset and fails closed when that capability is unavailable. "
        "A nested request wider than its delegated parent's CPU envelope is refused. Aliases: "
        "--cpuset, --pin. Absent leaves CPU placement unchanged.",
    )
    run_p.add_argument(
        "--max-mem",
        metavar="SPEC",
        default=None,
        help="RAM budget (e.g. 8G, 4096M): becomes the outer scope's MemoryMax (it can tighten the derived host "
        "boundary, never widen it) and derives a conservative model-based --max-steps ceiling; with explicit "
        "--max-steps, the tighter value wins. A nested run inherits its delegated parent's finite ceiling as "
        "the default and upper bound, enforced at the nested aggregate root",
    )
    run_p.add_argument(
        "--selected",
        metavar="TAG[,TAG...]",
        default=None,
        help="run the named step(s) and every dependency they require (comma-separated "
        "'group.job' tags). Errors if a named tag does not exist.",
    )
    run_p.add_argument(
        "--labels",
        metavar="LABEL[,LABEL...]",
        default=None,
        help="run every step carrying any named label, plus every dependency those steps require. "
        "Labels are independent of 'group.job' step tags. Errors if a label does not exist.",
    )
    run_p.add_argument(
        "--ignore-selected-deps",
        action="store_true",
        help="valid only with --selected: run only the named steps, dropping dependency edges "
        "to steps outside the selection because their outputs are assumed already present",
    )
    run_p.add_argument(
        "--args",
        default=None,
        metavar="STRING",
        help="forward these extra args to the test runner INSIDE a step. A step DECLARES that it "
        "accepts passthrough by placing the reserved token '{args}' in its cmd (it also picks the "
        "position); this STRING is substituted there verbatim (shell syntax, quote once). Use the "
        "--args=... form when the string starts with '-' (e.g. --args='-k test_xyz --verbose'). "
        "Combine with --selected to scope a step down to one test case, then --stress N to multiply "
        "it. Errors if no selected step declares '{args}'. A declared step with no --args runs "
        "with the token removed.",
    )
    run_p.add_argument(
        "--stress",
        type=int,
        default=None,
        metavar="N",
        help="duplicate the selected graph at generation into N disconnected components with no "
        "edges between copies. Named-resource scheduling is removed from the generated copies; "
        "--max-steps controls active copies; --max-cpus caps each copy's width and their shared "
        "outer CPU bandwidth. "
        "Reports the largest measured number of overlapping child processes, "
        "the per-copy PASS/FAIL RATIO (e.g. '7/10 passed'), and which copies failed. Combine with "
        "--selected to copy one suspect graph (e.g. --selected test.unit). Add "
        "--ignore-selected-deps to omit its dependencies. The ratio is the finding, so "
        "this implies --keep-going. N is still capped by the box memory budget (N x per-copy "
        "footprint must fit) and expansion may create at most 100,000 DAG nodes/control units. "
        "Each copy gets DAGRUN_COPY (zero-padded index, e.g. 03) and "
        "DAGRUN_COPIES (N) in its environment; interpolate the index into any "
        "output path, or N copies write one file and the last writer silently wins. If you "
        "are comparing the runs themselves, keep these out of the program under test: a "
        "process's environment sits on its initial stack, so a tool that hashes process "
        "state sees a per-copy variable as a difference.",
    )
    run_p.add_argument(
        "--perf-dir",
        metavar="DIR",
        default=None,
        help="write per-step + whole-run resource-usage CSVs into DIR (overrides the default "
        f"profile store and ${PROFILE_DIR_ENV})",
    )
    run_p.add_argument(
        "--no-profile",
        action="store_true",
        help="disable the default auto-logging profile store (do not append profile CSVs anywhere)",
    )
    run_p.add_argument(
        "--profile",
        action="store_true",
        help="after the run, print a per-step profile table (step | wall_s | user_s | sys_s | "
        "rss_hwm | oom | inner_jobs) to the terminal",
    )
    run_p.add_argument(
        "--profile-timeseries",
        metavar="DURATION",
        default=None,
        help="OPT-IN: sample each boxed step's cgroup CPU use over time at this interval "
        "(50ms..10s) and write a trace CSV under the profile directory",
    )
    run_p.add_argument(
        "--planner",
        choices=[p.value for p in Planner],
        default=Planner.GREEDY_LPT.value,
        help="dispatch-ordering planner: 'greedy-lpt' (default; longest est_duration first), "
        "'critical-path' (longest remaining est-weighted path first, i.e. highest bottom-level), "
        "or 'cpa' (measured-curve moldable allocator: choose each step's inner -j by balancing the "
        "critical path against the per-core area, then critical-path list-schedule at those widths)",
    )
    run_p.add_argument(
        "--show-plan",
        action="store_true",
        help="before running, print the plan (per-step est_duration + source, rss_estimate, "
        "bottom_level, the critical path, and the scheduled order)",
    )
    run_p.add_argument(
        "--no-profile-feedback",
        action="store_true",
        help="do NOT read the profile store to refine est_duration_s / rss_baseline_bytes at plan "
        "time; use only the DAG-authored hints (for reproducibility)",
    )
    run_p.add_argument(
        "--profile-memory-feedback",
        action="store_true",
        help="OPT-IN: additionally derive rss_baseline_bytes from the profile store's UNCENSORED peaks only. A peak that reached its applied memory.max proves the step used all it was allowed, not what it wanted, so such samples raise the estimate as a floor and never lower it; a step without enough uncensored evidence keeps its authored hint and the reason is printed",
    )
    run_p.add_argument(
        "--profile-sync",
        metavar="BACKEND",
        default=None,
        help="close the profiling feedback loop on EPHEMERAL CI: DOWNLOAD+merge the shared, "
        "constant-sized profile summary at start (seeding the planner) and merge-in this run's "
        "samples + UPLOAD at end. BACKEND is 'local:<dir>', 'git:<url>#<branch>[#<subdir>]' "
        "(atomic), or 'github-artifacts:<name>[#<owner/repo>]' (non-atomic). "
        "Independent of --perf-dir (the local CSV store still writes as usual).",
    )
    run_p.add_argument(
        "--profile-sync-direction",
        choices=["both", "download", "upload"],
        default="both",
        help="which half of --profile-sync to do: 'both' (default), 'download' (only seed the "
        "planner from the shared summary; do not upload), or 'upload' (only publish this run's "
        "samples; do not seed). --no-profile-feedback also suppresses the download seed.",
    )
    run_p.add_argument(
        "-k",
        "--keep-going",
        action="store_true",
        help="after a failure, continue launching independent ready steps and skip true dependents",
    )
    run_p.add_argument(
        "--run-timeout",
        type=int,
        default=None,
        metavar="SECONDS",
        help="OUTER wall budget for the WHOLE run. On breach the scheduler terminates every "
        "in-flight step's tree and still reports (rows written, verdict returned) instead of "
        "leaving the process to be killed from outside, which would take the evidence with it. "
        "Refuses to start if any step may run as long as the run itself.",
    )
    run_p.add_argument(
        "--admission",
        nargs="?",
        const="0",
        default=None,
        metavar="WAIT_S",
        help="HOST-WIDE memory admission (opt-in). Before any cgroup is brought up, reserve this "
        "run's --max-mem against a durable ledger every runner on the host shares, so two runs "
        "started a second apart cannot each take the same headroom. Three answers: GRANT (held "
        "for the run), QUEUE (it would fit on a quiet host -- says how many holders are ahead), "
        "REFUSE (bigger than the whole-host budget, so waiting can never help -- says the number "
        "to ask for instead). WAIT_S is how long to wait while queued (default 0 = report and "
        "exit 4 rather than wait; at most 86400). Requires --max-mem or a finite inherited "
        "delegated memory ceiling: admission needs a number, and guessing one would be worse "
        "than not gating.",
    )
    run_p.add_argument(
        "--allow-cgroup-failure",
        action="store_true",
        help="downgrade to a best-effort UNBOXED run (with a visible warning) instead of erroring "
        "when two-level cgroup-v2 + systemd --user scope boxing cannot be established",
    )
    run_p.add_argument(
        "--unsafe-no-cgroups",
        action="store_true",
        help="DELIBERATELY skip cgroup boxing entirely, even where it is available (no per-step "
        "memory/CPU/pids caps). The word 'unsafe' is intentional friction: unlike "
        "--allow-cgroup-failure (a capability fallback), this is an explicit opt-out that is "
        "logged loudly and should be reviewed. Use only when you have a specific reason not to box.",
    )
    run_p.add_argument(
        "--allow-unwise-nest-dagruns",
        action="store_true",
        help="allow this run even when it was launched from a dagrun step; this is an explicit "
        "exception for a known nested caller and should be removed by flattening that caller",
    )
    run_p.add_argument(
        "--small-default-cap",
        action="store_true",
        help="compatibility flag that explicitly reasserts the default SMALL forcing-function caps "
        "(1 core / 1 GiB / 10 s CPU) for steps that DECLARE NOTHING. The caps are already ON by "
        "default; an explicit per-step hint still wins.",
    )
    run_p.add_argument(
        "--cpu-timeout-multiplier",
        type=float,
        default=None,
        metavar="FACTOR",
        help="scale every step's canonical cpu_timeout by FACTOR on THIS platform (default 1.0 = "
        "no scaling). A CPU second is load-immune but not clock-immune, so identical work burns "
        "more CPU-seconds on a slower runner; this keeps ONE canonical budget in the graph and "
        "adapts enforcement per platform instead of maintaining a second, drifting table of "
        f"pre-multiplied numbers. Also settable per-lane via ${CPU_TIMEOUT_MULTIPLIER_ENV} "
        f"(and ${CPU_TIMEOUT_PLATFORM_ENV} for the label named in breach messages).",
    )
    run_p.add_argument("-v", dest="verbosity", action="count", default=1, help="-v: stream child output")
    run_p.add_argument("-q", "--quiet", action="store_true", help="quieter output")

    sweep_p = sub.add_parser(
        "sweep",
        allow_abbrev=False,
        formatter_class=_ColorHelp,
        help="measure parallel scaling for one step or a whole DAG",
        description="""Measure each selected step in isolation across inner-worker widths.

Two modes:
  Single-step dense mode (no --target-time): --step and --jobs are required.
  Target-time graph mode: visit selected nodes one at a time in stable topological
  order. Pass 1 always completes. A later pass starts only while time remains,
  and every started pass finishes even if that overruns the allowance.

Dependencies determine the visitation order; they are not run beside the measured
step. Each width is delivered through cmdtype, jobs_flag, and/or jobs_env.

Automatic widths (target-time mode with no --jobs):
  Pass 1 uses powers of two through the physical-core count, then the exact
  physical-core and logical-thread counts, limited by CPU affinity/cgroup quota.
  Example: 158 physical cores / 316 threads ->
  1,2,4,8,16,32,64,128,158,316.
  Later passes add integer midpoints of remaining gaps (for example, 48 between
  32 and 64) and rerun every width in the cumulative grid.

Output:
  Progress and speedup tables remain on the terminal. Profiling artifacts append
  to ./.dagrun/profiles/ by default. --output-dir changes only that artifact-store
  location; it does not redirect terminal or child-process output. A successful
  profiled sweep also refreshes profile_report.html there. Opt-in perf/wprof
  captures first select each measured step's sweet spot from this sweep's own
  uninstrumented data, then run separate trials under captures/; those trials
  never feed back into the scaling model.""",
    )
    sweep_p.add_argument(
        "--dag",
        required=True,
        metavar="FILE",
        help=(
            "DAG file; .yaml/.yml load as YAML, else JSON; files may use namespaced "
            "include fragments ('-' = self-contained JSON stdin only)"
        ),
    )
    sweep_p.add_argument(
        "--step",
        metavar="TAG",
        help="measure only this 'group.job' step; required without --target-time, optional otherwise",
    )
    sweep_p.add_argument(
        "--jobs",
        metavar="WIDTHS",
        help="initial inner-parallelism widths: bare N means every width 1..N; LO..HI is an "
        "inclusive dense range. Target mode also accepts sparse comma/range lists such as "
        "1,2,4,8 or 1,2,4..8. Omit only in target mode to use the topology grid described above. "
        "Delegated sweeps refuse explicit widths beyond the inherited CPU envelope.",
    )
    sweep_p.add_argument(
        "--target-time",
        metavar="DURATION",
        default=None,
        help="select graph-wide multi-pass mode with this soft wall-time allowance (bare seconds "
        "or ms/s/m/h suffix). Pass 1 and every started pass finish; this is not a timeout.",
    )
    sweep_p.add_argument(
        "--repeat",
        type=int,
        default=1,
        metavar="K",
        help="run every width K times; persist every sample and show the fastest wall time "
        "in the terminal table (default: 1)",
    )
    sweep_p.add_argument(
        "--output-dir",
        metavar="DIR",
        default=None,
        help="artifact directory: append profile CSVs, refresh scaling_model_*.json and the "
        "interactive profile_report.html, place time-series traces under DIR/traces/, and place "
        "perf/wprof sessions under DIR/captures/. "
        f"Overrides ${PROFILE_DIR_ENV} and ./.dagrun/profiles/; does not redirect terminal output.",
    )
    sweep_p.add_argument(
        "--perf-dir",
        metavar="DIR",
        default=None,
        help="backward-compatible alias for --output-dir",
    )
    sweep_p.add_argument(
        "--no-profile",
        action="store_true",
        help="write no local profile CSVs, scaling-model sidecar, interactive HTML report, "
        "time-series trace, or profiler capture",
    )
    sweep_p.add_argument(
        "--report-html",
        metavar="FILE",
        default=None,
        help="write the interactive historical profile report to FILE after a successful sweep "
        "(default: <profile-dir>/profile_report.html); failure at an explicitly requested FILE "
        "makes the sweep unsuccessful after preserving its measurements",
    )
    sweep_p.add_argument(
        "--no-report",
        action="store_true",
        help="keep profile CSV/model writes but do not refresh the interactive HTML report",
    )
    sweep_p.add_argument(
        "--profile-timeseries",
        metavar="DURATION",
        default=None,
        help="OPT-IN: sample each trial's cgroup CPU/thread activity every DURATION "
        "(50ms..10s); requires active cgroup-v2 and writes traces/<run_id>.csv in the profile store",
    )
    sweep_p.add_argument(
        "--perf-record",
        action="store_true",
        help="OPT-IN: after the uninstrumented sweep selects a step's sweet-spot width, run one "
        "separate perf-record trial there; this trial is not added to the scaling model",
    )
    sweep_p.add_argument(
        "--perf-window",
        metavar="DURATION",
        default=None,
        help="record only this centered hot interval through perf's acknowledged FIFO control "
        "interface (implies --perf-record; bare seconds or ms/s/m/h suffix)",
    )
    sweep_p.add_argument(
        "--wprof-window",
        metavar="DURATION",
        action="append",
        default=None,
        help="OPT-IN, repeatable: run one separate sweet-spot trial with a centered wprof window "
        "of this duration (typically 300ms..500ms); repeated durations must currently match",
    )
    sweep_p.add_argument(
        "--profiler-sudo",
        action="store_true",
        help="run requested perf/wprof tools through explicit non-interactive 'sudo -n'; dagrun "
        "never escalates profiler privileges unless this flag is present",
    )
    sweep_p.add_argument(
        "--allow-cgroup-failure",
        action="store_true",
        help="run UNBOXED with a warning when cgroup-v2 is unavailable; cgroup CPU, memory, "
        "throttling, and time-series evidence will be unavailable",
    )
    sweep_p.add_argument(
        "--unsafe-no-cgroups",
        action="store_true",
        help="DELIBERATELY run unboxed even when cgroup-v2 is available; profiling is less complete "
        "and --profile-timeseries cannot be used",
    )
    sweep_p.add_argument(
        "-v",
        dest="verbosity",
        action="count",
        default=0,
        help="-v shows each trial's captured final line; -vv streams child stdout/stderr",
    )

    plan_p = sub.add_parser(
        "plan",
        allow_abbrev=False,
        help="show the plan: per-step est_duration (+ source), rss_estimate, bottom-level, the "
        "critical path, and the scheduled order (does NOT run anything)",
        description="show the plan: per-step est_duration (+ source), rss_estimate, bottom-level, the "
        "critical path, and the scheduled order (does NOT run anything)",
    )
    plan_p.add_argument(
        "--dag",
        required=True,
        metavar="FILE",
        help=(
            "DAG file; .yaml/.yml load as YAML, else JSON; files may use namespaced "
            "include fragments ('-' = self-contained JSON stdin only)"
        ),
    )
    plan_p.add_argument(
        "--planner",
        choices=[p.value for p in Planner],
        default=Planner.GREEDY_LPT.value,
        help="dispatch-ordering planner: 'greedy-lpt' (default), 'critical-path', or 'cpa' "
        "(measured-curve moldable allocator: pick each step's inner -j by balancing the critical "
        "path against the per-core area, then list-schedule)",
    )
    plan_p.add_argument(
        "--max-mem",
        metavar="SPEC",
        default=None,
        help="RAM budget (e.g. 8G, 4096M) for the 'cpa' planner's allocation: a step is not "
        "widened if the DAG's modeled worst-case concurrent footprint would exceed it (ignored by "
        "the other planners)",
    )
    plan_p.add_argument(
        "--max-steps",
        type=_positive_i64,
        default=None,
        metavar="N",
        help="maximum active steps whose reachable concurrent memory CPA must fit "
        "(default: the CPA core budget)",
    )
    plan_p.add_argument(
        "--format",
        choices=["text", "json"],
        default="text",
        help="output format: 'text' (human table, default) or 'json' (canonical machine-readable)",
    )
    plan_p.add_argument(
        "--perf-dir",
        metavar="DIR",
        default=None,
        help="read the profile store from DIR (overrides the default store and "
        f"${PROFILE_DIR_ENV}) when deriving learned estimates",
    )
    plan_p.add_argument(
        "--no-profile-feedback",
        action="store_true",
        help="ignore the profile store; show only the DAG-authored hints",
    )

    for cmd, helptext in (
        ("list", "list the steps"),
        ("ascii", "draw the DAG as ASCII art"),
        ("dot", "emit Graphviz DOT (pipe to `dot -Tsvg`)"),
        ("json", "re-emit the DAG as canonical JSON"),
        ("yaml", "re-emit the DAG as YAML"),
    ):
        sp = sub.add_parser(cmd, allow_abbrev=False, help=helptext, description=helptext)
        sp.add_argument(
            "--dag",
            required=True,
            metavar="FILE",
            help=(
                "DAG file; .yaml/.yml load as YAML, else JSON; files may use namespaced "
                "include fragments ('-' = self-contained JSON stdin only)"
            ),
        )
        if cmd in ("ascii", "dot"):
            sp.add_argument(
                "--labels",
                metavar="LABEL[,LABEL...]",
                default=None,
                help=(
                    "show only steps carrying any named label, plus every dependency they "
                    "require (the same selection as `run --labels`)"
                ),
            )
            sp.add_argument(
                "--group-by",
                choices=GROUP_BY_KEYS,
                default=None,
                help=(
                    "collapse steps to one node per `group`, showing its step count; an edge per "
                    "(upstream group -> group) pair shows how many step dependencies it merges"
                ),
            )

    summary_p = sub.add_parser(
        "summary",
        allow_abbrev=False,
        help="inspect / build / merge / plan-from the constant-sized mergeable profile SUMMARY "
        "(the artifact --profile-sync uploads+downloads to close the ephemeral-CI feedback loop)",
        description="inspect / build / merge / plan-from the constant-sized mergeable profile SUMMARY "
        "(the artifact --profile-sync uploads+downloads to close the ephemeral-CI feedback loop)",
    )
    summary_sub = summary_p.add_subparsers(dest="summary_command", metavar="<action>")

    sb_build = summary_sub.add_parser(
        "build",
        allow_abbrev=False,
        help="build a summary JSON from a profile store (CSV) for the current identity",
        description="build a summary JSON from a profile store (CSV) for the current identity",
    )
    sb_build.add_argument(
        "--perf-dir",
        metavar="DIR",
        default=None,
        help="read the profile store CSV from DIR (else the default store / $"
        f"{PROFILE_DIR_ENV})",
    )
    sb_build.add_argument("--out", metavar="FILE", default=None, help="write JSON here (else stdout)")
    sb_build.add_argument(
        "--reservoir-cap",
        type=_positive_i64,
        default=DEFAULT_RESERVOIR_K,
        metavar="K",
        help="max samples kept per (step, inner_jobs, workload_digest) bucket "
        f"(default {DEFAULT_RESERVOIR_K})",
    )

    sb_merge = summary_sub.add_parser(
        "merge",
        allow_abbrev=False,
        help="merge one or more summary JSON files into one (order-independent) on stdout",
        description="merge one or more summary JSON files into one (order-independent) on stdout",
    )
    sb_merge.add_argument("files", nargs="+", metavar="FILE", help="summary JSON files to merge")
    sb_merge.add_argument("--out", metavar="FILE", default=None, help="write JSON here (else stdout)")
    sb_merge.add_argument(
        "--reservoir-cap", type=_positive_i64, default=DEFAULT_RESERVOIR_K, metavar="K",
        help="max samples per workload-specific bucket after merge "
        f"(default {DEFAULT_RESERVOIR_K})",
    )

    sb_plan = summary_sub.add_parser(
        "plan",
        allow_abbrev=False,
        help="build a plan from a summary JSON and DAG",
        description="build a plan from a summary JSON and DAG",
    )
    sb_plan.add_argument("--summary", required=True, metavar="FILE", help="summary JSON file")
    sb_plan.add_argument(
        "--dag", required=True, metavar="FILE",
        help=(
            "DAG file; .yaml/.yml load as YAML, else JSON; files may use namespaced "
            "include fragments ('-' = self-contained JSON stdin only)"
        ),
    )
    sb_plan.add_argument(
        "--planner", choices=[p.value for p in Planner], default=Planner.GREEDY_LPT.value,
        help="dispatch-ordering planner (see `plan --help`)",
    )
    sb_plan.add_argument("--max-mem", metavar="SPEC", default=None, help="RAM budget for cpa planner")
    sb_plan.add_argument(
        "--max-steps",
        type=_positive_i64,
        default=None,
        metavar="N",
        help="maximum active steps whose concurrent memory the cpa planner must fit",
    )
    sb_plan.add_argument(
        "--format", choices=["text", "json"], default="text", help="output format (default text)"
    )

    sb_stats = summary_sub.add_parser(
        "stats",
        allow_abbrev=False,
        help="print bucket_count / total_samples / max_bucket_samples (the bounded-size witness)",
        description="print bucket_count / total_samples / max_bucket_samples (the bounded-size witness)",
    )
    sb_stats.add_argument("file", metavar="FILE", help="summary JSON file")

    pin_p = sub.add_parser(
        "pin-run",
        allow_abbrev=False,
        help="reserve K collision-free cores, box a command onto them, run it, release on exit",
        description="reserve K collision-free cores, box a command onto them, run it, release on exit",
    )
    pin_p.add_argument(
        "--cores",
        type=_positive_int,
        required=True,
        metavar="K",
        help="reserve K disjoint cores via the durable cross-process ledger (never collides with "
        "a concurrent pin-run/run --cores), constrain the WHOLE command subtree to them, run the "
        "command, and RELEASE on exit (incl. failure). Assigned cores print to stderr; exit code "
        "== the command's. Dead holders are reclaimed automatically.",
    )
    pin_p.add_argument(
        "--tag",
        default="",
        metavar="STR",
        help="label recorded in the ledger for this reservation (for inspection/debugging)",
    )
    pin_p.add_argument(
        "cmd",
        nargs=argparse.REMAINDER,
        metavar="-- CMD [ARGS...]",
        help="the command to run pinned (put it after '--')",
    )

    box_p = sub.add_parser(
        "box",
        allow_abbrev=False,
        help="box ONE command with --mem/--timeout/--cores, without writing a DAG file",
        description=(
            "Run ONE command under the same cgroup-v2 boxing a DAG step gets, without writing a "
            "DAG file. Boxing one ad-hoc command is this tool's primary purpose, and until now "
            "the only way to do it was to hand-write a singleton-DAG JSON file."
        ),
    )
    box_p.add_argument(
        "--mem",
        metavar="SPEC",
        default=None,
        help="RAM ceiling for the boxed command (e.g. 512M, 8G). Applied BOTH as the outer "
        "scope's MemoryMax and as the command's own inner memory.max, so a breach is an OOM kill "
        "inside the box rather than pressure on the host. Absent uses the small default cap.",
    )
    box_p.add_argument(
        "--timeout",
        type=_positive_i64,
        default=None,
        metavar="SECS",
        help="WALL-clock ceiling for the boxed command in seconds. The CPU-time ceiling is "
        "derived from it (--timeout x --cores), so the wall bound is the one that fires and the "
        "small 10-second per-step CPU floor cannot cut an honest command short.",
    )
    box_p.add_argument(
        "--cores",
        "-j",
        "--max-cpus",
        dest="cores",
        type=_positive_int,
        default=None,
        metavar="K",
        help="CPU BANDWIDTH for the boxed command: cpu.max of K cores inside the box and an "
        "outer CPU-bandwidth budget of K. NOTE this is deliberately NOT what `run --cores` means "
        "-- that flag is a hard cpuset PIN which fails closed without an exact cgroup cpuset. "
        "Boxing one command should not require that capability, so this is a bandwidth cap.",
    )
    box_p.add_argument(
        "--label",
        default=None,
        metavar="NAME",
        help="name for the boxed step in output and in the per-step evidence log "
        "(default: the command's basename)",
    )
    box_p.add_argument(
        "--perf-dir",
        metavar="DIR",
        default=None,
        help="write the boxed command's resource-usage CSVs into DIR",
    )
    box_p.add_argument(
        "--allow-cgroup-failure",
        action="store_true",
        help="downgrade to a best-effort UNBOXED run when cgroup boxing is unavailable, instead "
        "of refusing (exit 3)",
    )
    box_p.add_argument(
        "-q", "--quiet", action="store_true", help="suppress the per-step summary lines"
    )
    box_p.add_argument(
        "command_argv",
        nargs=argparse.REMAINDER,
        metavar="-- CMD [ARGS...]",
        help="the command to box (put it after '--')",
    )

    sub.add_parser(
        "quickstart", allow_abbrev=False, help="print a self-contained getting-started guide",
        description="print a self-contained getting-started guide",
    )
    sub.add_parser(
        "capabilities",
        allow_abbrev=False,
        help="print the machine-readable enforcement-capability manifest",
        description="print the machine-readable enforcement-capability manifest",
    )
    return parser


def _load(dag_arg: str) -> DagConfig:
    if dag_arg == "-":
        # stdin has no filename to auto-detect from: default to JSON.
        source = "<stdin>"
        text = sys.stdin.read()
        load_text = dag_from_json
    else:
        return dag_from_path(Path(dag_arg))
    try:
        return load_text(text)
    except DagJsonError as exc:
        detail = str(exc).removeprefix("<root>: ")
        raise DagJsonError(f"{source}: {detail}") from exc


def _git_sha() -> str:
    """Best-effort git SHA of the current working directory's repo, or ``"unknown"``.

    Used only to stamp perf-log rows (``--perf-dir``); it must never fail the run, so any
    error (not a repo, git absent, timeout) degrades to ``"unknown"``."""
    try:
        proc = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return "unknown"
    if proc.returncode != 0:
        return "unknown"
    return proc.stdout.strip() or "unknown"


# --------------------------------------------------------------------------- profile store
def _resolve_profile_dir(
    perf_dir_arg: str | None, no_profile: bool
) -> tuple[str | None, str]:
    """Resolve the effective profile-store directory and a human label for its source.

    Precedence (Feature D): ``--no-profile`` disables logging entirely (returns ``(None, ...)``);
    otherwise an explicit ``--perf-dir`` wins; otherwise ``$DAGRUN_PROFILE_DIR``;
    otherwise the repo-local default ``./.dagrun/profiles/``. Auto-logging is ON by
    default so profiling data lands somewhere obvious without any flag; the caller MUST surface
    where it was appended (No Silent Failure)."""
    if no_profile:
        return None, "disabled"
    if perf_dir_arg:
        return perf_dir_arg, "--perf-dir"
    env = os.environ.get(PROFILE_DIR_ENV)
    if env:
        return env, f"${PROFILE_DIR_ENV}"
    return DEFAULT_PROFILE_DIR, "default"


def _resolve_feedback_dir(perf_dir_arg: str | None, no_feedback: bool) -> str | None:
    """The directory the plan-time FEEDBACK reader loads the profile store from, or ``None`` when
    feedback is off.

    Independent of ``--no-profile`` (which only governs WRITING): reading the store to refine
    estimates is a separate concern. Precedence mirrors the write path minus the disable:
    ``--no-profile-feedback`` turns it off; else ``--perf-dir``; else
    ``$DAGRUN_PROFILE_DIR``; else the repo-local default store."""
    if no_feedback:
        return None
    if perf_dir_arg:
        return perf_dir_arg
    env = os.environ.get(PROFILE_DIR_ENV)
    if env:
        return env
    return DEFAULT_PROFILE_DIR


def _apply_memory_feedback(
    cfg: DagConfig, authored: DagConfig, feedback_dir: str | None, enabled: bool
) -> DagConfig:
    """Apply censoring-aware profile memory feedback to ``cfg``, reporting every decision.

    OFF unless the caller asked for it. Default plan-time feedback treats an authored
    ``rss_baseline_bytes`` as a floor: it may raise that value, but cannot shrink the next run's
    enforced cap from sparse history. This path is the separate opt-in allowed to lower the
    authored value, and only from enough uncensored observations plus a margin.

    ``cfg`` is the config the ordinary plan has ALREADY been applied to and ``authored`` is the
    same config before that, which is what lets a decline restore the LARGER of the baseline its
    author wrote and the peak the store proves it has already reached. That also replaces a
    larger ordinary-feedback estimate only when this stricter path has enough evidence to do so.

    Every step the store knows about is reported on stderr, including the ones that did NOT
    move and why, because "the cap did not change" and "the store had nothing usable to say"
    look identical from the outside otherwise.

    ``--no-profile-feedback`` turns the store reader off entirely, which makes this flag a
    no-op. That combination is legal but empty, and it is announced rather than obeyed in
    silence: a caller who asked for a learned cap by name and got the authored one has been
    told something untrue by omission.
    """
    if not enabled:
        return cfg
    baselines = {step.tag: step.hint.rss_baseline_bytes for step in authored.steps}
    if feedback_dir is None:
        # Nothing to undo: with the reader off, the plan never learned a baseline either.
        print(
            f"{PROG}: --profile-memory-feedback: --no-profile-feedback disables the profile-store "
            "reader, so no estimate is derived and every authored hint is used as written",
            file=sys.stderr,
        )
        return cfg
    admissions = load_memory_admissions(feedback_dir)
    if not admissions:
        print(
            f"{PROG}: --profile-memory-feedback: no profile store for this machine/container "
            f"identity under {feedback_dir}; every authored hint is retained",
            file=sys.stderr,
        )
        return apply_memory_admissions(cfg, {}, baselines)
    tags = {step.tag for step in cfg.steps}
    for tag in sorted(tags & admissions.keys()):
        print(_memory_admission_line(admissions[tag], baselines.get(tag)), file=sys.stderr)
    return apply_memory_admissions(cfg, admissions, baselines)


def _memory_admission_line(admission: MemoryAdmission, authored: int | None) -> str:
    """One human-readable line stating the decision AND the evidence behind it.

    ``authored`` is the baseline the step's author wrote, which is what makes the decline half
    of this line specific: a decline that nevertheless raises the step to a proven floor says
    so and names the number, because "keeping the authored hint" would be as untrue there as
    retaining a larger ordinary estimate while claiming the authored value had been restored.
    """
    floor = admission.proven_floor_bytes()
    if admission.source == "profile":
        verdict = f"rss_baseline_bytes={admission.rss_baseline_bytes}"
    elif floor is not None and floor > (authored or 0):
        verdict = f"no estimate; rss_baseline_bytes={floor}, the proven floor"
    else:
        verdict = "keeping the authored hint"
    return (
        f"{PROG}: --profile-memory-feedback: {admission.step}: {verdict} "
        f"[{admission.uncensored_samples} uncensored, {admission.censored_samples} censored, "
        f"{admission.unknown_samples} unprovenanced of {admission.samples}; {admission.reason}]"
    )


def _build_feedback_plan(
    cfg: DagConfig,
    feedback_dir: str | None,
    planner: Planner,
    *,
    core_budget: int | None = None,
    mem_budget: int | None = None,
    max_steps: int | None = None,
) -> Plan:
    """Load the profile store (when feedback is on) and build the plan for ``planner``.

    With ``feedback_dir`` set, learned durations refine each step and learned RSS may raise, but
    never lower, an authored baseline; per-step parallel-speedup curves are attached for the plan
    display. With ``feedback_dir`` ``None`` the plan reflects the DAG hints only. ``core_budget``
    (``P``) bounds every displayed speedup recommendation and drives the CPA
    allocator under ``--planner cpa``; ``mem_budget`` applies only to CPA allocation.
    """
    if feedback_dir is not None:
        machine_id, container_class = feedback_identity()
        workloads = {
            step.tag: workload_digest(
                step, cfg.default_jobs_flag, cfg.default_jobs_env
            )
            for step in cfg.steps
        }
        samples = load_step_samples(
            feedback_dir, machine_id, container_class, workloads
        )
        speedups = load_step_speedups(
            feedback_dir, machine_id, container_class, workloads
        )
    else:
        samples = {}
        speedups = {}
    return build_plan(
        cfg,
        samples,
        planner=planner,
        speedups=speedups,
        core_budget=core_budget,
        mem_budget=mem_budget,
        max_steps=max_steps,
    )


def _planning_budgets(
    planner: Planner,
    max_mem_arg: str | None,
    max_cpus: int | None = None,
    effective_max_mem_bytes: int | None = None,
) -> tuple[int | None, int | None]:
    """Resolve the ``(core_budget, mem_budget)`` used to build a truthful plan.

    A run supplies its resolved total ``max_cpus`` under every planner, so ``--show-plan`` cannot
    recommend a width the run will throttle. A standalone ordering-only plan has no run boundary
    and remains unbounded; standalone CPA resolves :func:`container_core_budget` because width
    allocation requires one. ``mem_budget`` is parsed only for CPA.
    """
    core_budget = (
        max(1, max_cpus)
        if max_cpus is not None
        else (max(1, container_core_budget()) if planner is Planner.CPA else None)
    )
    mem_budget = (
        effective_max_mem_bytes
        if planner is Planner.CPA and effective_max_mem_bytes is not None
        else (parse_size(max_mem_arg) if planner is Planner.CPA and max_mem_arg else None)
    )
    return core_budget, mem_budget


def _build_plan_from_summary(
    cfg: DagConfig,
    summary: Summary,
    planner: Planner,
    *,
    core_budget: int | None = None,
    mem_budget: int | None = None,
    max_steps: int | None = None,
) -> Plan:
    """Build a plan whose learned estimates come from the mergeable SUMMARY (rather than a CSV
    store). The reader half of the sync feature: the estimates are recomputed from the summary's
    reservoirs via the same estimator core the CSV path uses, so a summary and the store it came
    from plan identically (until a bucket is subsampled)."""
    workloads = {
        step.tag: workload_digest(step, cfg.default_jobs_flag, cfg.default_jobs_env)
        for step in cfg.steps
    }
    samples = summarylib.step_samples_from_summary(summary, workloads)
    speedups = summarylib.step_speedups_from_summary(
        summary, workload_digests=workloads
    )
    return build_plan(
        cfg,
        samples,
        planner=planner,
        speedups=speedups,
        core_budget=core_budget,
        mem_budget=mem_budget,
        max_steps=max_steps,
    )


def _rows_to_str(rows: Sequence[Mapping[str, object]]) -> list[dict[str, str]]:
    """Stringify heterogeneous per-step profile rows to the ``str -> str`` shape the sample
    extractor parses, so this run's ``result.step_profile_rows`` funnel through the SAME
    :func:`~dagrun.estimates.sample_from_row` path as CSV cells (one code path)."""
    return [{str(k): "" if v is None else str(v) for k, v in row.items()} for row in rows]


def _local_store_summary(
    feedback_dir: str | None, machine_id: str, container_class: str
) -> Summary:
    """Build a summary from THIS machine's local CSV store (its own not-yet-uploaded history), so a
    persistent dev box's local runs also seed the planner alongside the downloaded shared summary.
    Empty when feedback is off or the store is absent."""
    if feedback_dir is None:
        return summarylib.empty(machine_id, container_class)
    from dagrun.estimates import _load_store

    loaded = _load_store(feedback_dir, machine_id, container_class)
    if loaded is None:
        return summarylib.empty(machine_id, container_class)
    rows, affinity = loaded
    return summarylib.summary_from_rows(rows, machine_id, container_class, affinity)


def _sync_seed_plan(
    cfg: DagConfig,
    backend: synclib.SyncBackend | None,
    feedback_dir: str | None,
    planner: Planner,
    do_download: bool,
    *,
    core_budget: int | None,
    mem_budget: int | None,
    max_steps: int | None = None,
) -> Plan | None:
    """DOWNLOAD the shared summary, MERGE it with this machine's local store, and build the plan from
    the result. Returns the seeded plan, or ``None`` to fall back to the normal CSV-feedback plan
    (sync off / download disabled / a backend failure). A backend failure degrades LOUDLY (a
    warning) rather than failing the run — the loop is best-effort, but never silent."""
    if backend is None or not do_download:
        return None
    machine_id, container_class = feedback_identity()
    try:
        downloaded = backend.download(machine_id, container_class)
    except synclib.SyncError as exc:
        print(
            f"{PROG}: --profile-sync: download failed, planning from local store only ({exc})",
            file=sys.stderr,
        )
        return None
    local = _local_store_summary(feedback_dir, machine_id, container_class)
    try:
        seed = summarylib.merge(downloaded, local)
    except summarylib.SummaryError as exc:
        print(f"{PROG}: --profile-sync: {exc}; planning from local store only", file=sys.stderr)
        return None
    buckets, total, _largest = summarylib.summary_stats(seed)
    print(
        f"{PROG}: --profile-sync: seeded planner from {backend.describe()} "
        f"({buckets} buckets, {total} samples for {machine_id}/{container_class})",
        file=sys.stderr,
    )
    return _build_plan_from_summary(
        cfg,
        seed,
        planner,
        core_budget=core_budget,
        mem_budget=mem_budget,
        max_steps=max_steps,
    )


def _rows_with_observation_ids(
    rows: Sequence[Mapping[str, object]], run_id: str
) -> list[dict[str, str]]:
    """Stamp raw scheduler rows with the execution identity used by their local CSV copies.

    A DAG executes a tag at most once, and summary deduplication is per ``(step, width)`` bucket, so
    the local store's run id is also the stable observation id for that bucket. Reusing it here is
    what lets a downloaded upload deduplicate against the same row read from local history.
    """

    identified: list[dict[str, str]] = []
    for row in _rows_to_str(rows):
        observation_id = row.get("observation_id") or row.get("run_id") or run_id
        identified.append({**row, "observation_id": observation_id})
    return identified


def _sync_upload(
    backend: synclib.SyncBackend,
    rows: Sequence[Mapping[str, object]],
    *,
    local_run_id: str | None = None,
) -> None:
    """Merge THIS run's per-step samples into the shared summary and publish them via ``backend``.
    Degrades LOUDLY on failure (a warning) so the run's own exit code is preserved but the skip is
    never silent (No Silent Failure)."""
    machine_id, container_class = feedback_identity()
    from dagrun.estimates import _affinity_width
    from dagrun.perflog import new_run_id

    observation_run_id = local_run_id or new_run_id()
    upload_rows = _rows_with_observation_ids(rows, observation_run_id)
    delta = summarylib.summary_from_rows(
        upload_rows, machine_id, container_class, _affinity_width(container_class)
    )
    try:
        merged = backend.publish(delta)
    except synclib.SyncError as exc:
        print(f"{PROG}: --profile-sync: upload failed ({exc})", file=sys.stderr)
        return
    buckets, total, largest = summarylib.summary_stats(merged)
    print(
        f"{PROG}: --profile-sync: published this run's samples to {backend.describe()} "
        f"(summary now {buckets} buckets, {total} samples, <= {largest}/bucket)",
        file=sys.stderr,
    )


def _report_profile_written(perf_dir: str, source: str) -> None:
    """Print one visible line naming where profile CSVs were appended (No Silent Failure).

    Lists EXACTLY the files this run/sweep wrote (the deterministic :func:`store_paths` set,
    filtered to files that exist), not a glob of the whole store — a persistent store also holds
    prior runs' other-``container_class`` CSVs for the same machine, and globbing would over-report
    files this run never touched."""
    from dagrun.perflog import store_paths

    written = sorted(str(p) for p in store_paths(perf_dir) if p.exists())
    if not written:
        print(f"{PROG}: WARNING no profile CSVs were written under {perf_dir}", file=sys.stderr)
        return
    if source == "--output-dir":
        print(f"{PROG}: profile artifacts written under {perf_dir}:", file=sys.stderr)
    elif source == "--perf-dir":
        print(f"{PROG}: perf CSVs written under {perf_dir}:", file=sys.stderr)
    else:
        origin = "default profile store" if source == "default" else f"profile store ({source})"
        print(
            f"{PROG}: profile data appended to the {origin} at {perf_dir} "
            f"(override with --perf-dir or ${PROFILE_DIR_ENV}; disable with --no-profile):",
            file=sys.stderr,
        )
    for path in written:
        print(f"  {path}", file=sys.stderr)


def _refresh_scaling_model(perf_dir: str, cfg: DagConfig) -> None:
    """Refresh the derived model sidecar from the authoritative raw profile CSV."""

    from dagrun import perflog

    machine_id = perflog.machine_id()
    container_class = perflog.container_class()
    try:
        workloads = {
            step.tag: workload_digest(
                step, cfg.default_jobs_flag, cfg.default_jobs_env
            )
            for step in cfg.steps
        }
        speedups = load_step_speedups(
            perf_dir, machine_id, container_class, workloads
        )
        path = write_scaling_model(
            perf_dir, machine_id, container_class, speedups, workloads
        )
    except (OSError, UnicodeError) as exc:
        print(
            f"{PROG}: WARNING could not refresh derived scaling model under {perf_dir}: {exc}",
            file=sys.stderr,
        )
        return
    print(
        f"{PROG}: derived scaling model refreshed at {path} "
        "(rebuildable from the profile CSVs)",
        file=sys.stderr,
    )


def _refresh_profile_report(
    perf_dir: str, cfg: DagConfig, dag_label: str, report_html: str | None
) -> bool:
    """Refresh the standalone history explorer from the authoritative profile store."""
    from dagrun.profile_report import (
        PROFILE_REPORT_FILENAME,
        ReportDataError,
        generate_report_for_config,
    )

    output_path = (
        Path(report_html) if report_html is not None else Path(perf_dir) / PROFILE_REPORT_FILENAME
    )
    try:
        data = generate_report_for_config(
            Path(perf_dir),
            cfg,
            output_path,
            dag_label=dag_label,
            title="dagrun profile history",
        )
    except (OSError, UnicodeError, ReportDataError) as exc:
        print(
            f"{PROG}: WARNING could not refresh interactive profile report at "
            f"{output_path}: {exc}",
            file=sys.stderr,
        )
        return False
    records = data.get("records")
    traces = data.get("traces")
    record_count = len(records) if isinstance(records, list) else 0
    trace_count = len(traces) if isinstance(traces, list) else 0
    print(
        f"{PROG}: interactive profile report refreshed at {output_path} "
        f"({record_count} aggregate sample(s), {trace_count} time-series run(s))",
        file=sys.stderr,
    )
    return True


# --------------------------------------------------------------------------- selected steps
class _SelectedError(Exception):
    """A ``--selected`` / ``--step`` selection referenced a tag that does not exist."""


def _parse_tag_list(raw: str) -> list[str]:
    """Split a comma-separated ``group.job`` tag list, dropping empty entries."""
    return [tag.strip() for tag in raw.split(",") if tag.strip()]


def _select_steps(
    cfg: DagConfig, tags: Sequence[str], *, ignore_selected_deps: bool = False
) -> DagConfig:
    """Return the named steps and, by default, every dependency they require.

    ``ignore_selected_deps`` keeps only the named steps and drops dependency edges to steps outside
    the selection because their outputs are assumed already present. Edges among selected steps are
    preserved. Registration order is preserved in both modes so both language builds agree. Raises
    :class:`_SelectedError` if any named tag does not exist."""
    by_tag = cfg.by_tag()
    unknown = [tag for tag in tags if tag not in by_tag]
    if unknown:
        known = ", ".join(sorted(by_tag)) or "(none)"
        raise _SelectedError(
            f"--selected: unknown step tag(s): {', '.join(unknown)}. Known tags: {known}"
        )
    selected = set(tags)
    if not ignore_selected_deps:
        dependencies = transitive_deps(cfg.steps)
        for tag in tags:
            selected.update(dependencies[tag])
    new_steps = tuple(
        (
            dataclasses.replace(step, deps=[dep for dep in step.deps if dep in selected])
            if ignore_selected_deps
            else step
        )
        for step in cfg.steps
        if step.tag in selected
    )
    return dataclasses.replace(cfg, steps=new_steps)


def _select_steps_by_labels(cfg: DagConfig, labels: Sequence[str]) -> DagConfig:
    """Return every step carrying any requested label and its dependency ancestry."""
    known = {label for step in cfg.steps for label in step.labels}
    unknown = [label for label in labels if label not in known]
    if unknown:
        known_text = ", ".join(sorted(known)) or "(none)"
        raise _SelectedError(
            f"--labels: unknown label(s): {', '.join(unknown)}. Known labels: {known_text}"
        )
    tags = [step.tag for step in cfg.steps if any(label in step.labels for label in labels)]
    return _select_steps(cfg, tags)


# --------------------------------------------------------------------------- --args passthrough
#: Reserved token a step places in its ``cmd`` to DECLARE it accepts CLI passthrough args (and to
#: pick WHERE they go). ``--args STRING`` is substituted here verbatim; a declared step run without
#: ``--args`` has the token removed so it stays runnable. A plain string in the existing ``cmd``
#: field, so DAGs round-trip byte-identically across the Python/Rust builds (no schema change).
ARGS_PLACEHOLDER = "{args}"


class _ArgsError(Exception):
    """``--args`` was given but no selected step declares the ``{args}`` placeholder."""


def _apply_passthrough_args(cfg: DagConfig, args: str | None) -> DagConfig:
    """Forward ``--args`` into every selected step that DECLARES the ``{args}`` placeholder.

    A step opts in by putting ``{args}`` in its ``cmd`` (the declaration, and the exact position).
    Given ``--args STRING`` the token is replaced verbatim (shell syntax, quoted once by the user);
    with no ``--args`` the token is removed so the declared step still runs. Steps without the token
    are untouched. Raises :class:`_ArgsError` when ``--args`` is given but NO step declares it, so a
    forwarded selection is never silently dropped (No Silent Failure)."""
    replacement = args if args is not None else ""
    any_declared = any(ARGS_PLACEHOLDER in step.cmd for step in cfg.steps)
    if args is not None and not any_declared:
        raise _ArgsError(
            f"--args was given but no selected step declares the {ARGS_PLACEHOLDER!r} placeholder "
            "in its cmd. Add {args} to the step's cmd where the extra args should go, or scope the "
            "selection (--selected) to a step that accepts them."
        )
    if not any_declared:
        return cfg  # nothing to substitute; leave the DAG byte-identical
    new_steps = tuple(
        dataclasses.replace(step, cmd=step.cmd.replace(ARGS_PLACEHOLDER, replacement).strip())
        if ARGS_PLACEHOLDER in step.cmd
        else step
        for step in cfg.steps
    )
    return dataclasses.replace(cfg, steps=new_steps)


# --------------------------------------------------------------------------- --stress fan-out
def _stress_suffix(index: int, count: int) -> str:
    """Zero-padded copy suffix, e.g. copy 3 of 10 -> ``"#03"``. The width tracks ``count`` so the
    copy tags sort lexicographically in copy order."""
    width = len(str(count))
    return f"#{index:0{width}d}"


#: Zero-padded index of this copy under ``--stress N``, e.g. ``"03"`` for copy 3 of 10.
#: Unset when the graph was not multiplied, so a command can tell one run from a copy.
STRESS_COPY_ENV = "DAGRUN_COPY"

#: The ``N`` of ``--stress N``, so a command can size a split without being told twice.
STRESS_COPIES_ENV = "DAGRUN_COPIES"


def _expand_stress(cfg: DagConfig, n: int) -> DagConfig:
    """Return a DAG with every step of ``cfg`` DUPLICATED into ``n`` independent copies (shards).

    Each copy has a distinct ``#NN`` suffix and its internal dependency edges point only to steps
    in that same copy. There are no edges between copies. Named-resource scheduling is removed
    from the generated graph so ``-j`` governs how many copied steps the scheduler starts at once;
    stress runs deliberately permit over-subscription. ``n <= 1`` is a no-op.

    The top-level policy travels with the expanded step list by construction, via
    :meth:`DagConfig.with_steps` — this is one of the two places in the product that rebuilds a
    ``DagConfig`` around new steps, and writing that as a literal is how the dropped-field bug in
    #21 scarce-resource-deadlock gets written every time. ``resource_caps`` is then cleared
    DELIBERATELY and visibly."""
    if n <= 1:
        return cfg
    new_steps: list[Step] = []
    for index in range(1, n + 1):
        suffix = _stress_suffix(index, n)
        for step in cfg.steps:
            new_steps.append(
                dataclasses.replace(
                    step,
                    job=f"{step.job}{suffix}",
                    deps=[f"{dep}{suffix}" for dep in step.deps],
                    hint=dataclasses.replace(step.hint, resources={}),
                    # Zero-padded to the same width as the job suffix, so a path built from
                    # the index sorts in copy order like the tags do. Every copy runs the
                    # SAME cmd, so without this the command cannot choose a distinct output
                    # path: N copies write one file, the last writer wins, nothing errors,
                    # and the result is one sample wearing the label of N. The #NN suffix
                    # cannot serve -- it is part of the job NAME, which the command never
                    # sees -- and DAGRUN_STEP is an ownership nonce (pid:counter:time_ns),
                    # so it is unstable across reruns and is not an index.
                    env={
                        **step.env,
                        STRESS_COPY_ENV: f"{index:0{len(str(n))}d}",
                        STRESS_COPIES_ENV: str(n),
                    },
                )
            )
    return dataclasses.replace(cfg.with_steps(new_steps), resource_caps={})


def _stress_expansion_guard(cfg: DagConfig, n: int) -> int:
    """Refuse stress fan-out that would allocate an excessive generated graph.

    Empty graphs count one control-plane unit per copy so a huge ``n`` cannot burn time looping
    without creating steps. Arithmetic saturates to the shared signed-i64 domain.
    """
    nodes_per_copy = max(1, len(cfg.steps))
    generated = min(2**63 - 1, nodes_per_copy * n)
    if generated <= MAX_STRESS_GENERATED_NODES:
        return 0
    print(
        f"{PROG}: --stress {n}: REFUSED — expansion would create {generated} generated DAG "
        f"nodes/control units, exceeding safety limit {MAX_STRESS_GENERATED_NODES}; narrow "
        "--selected or lower --stress",
        file=sys.stderr,
    )
    return 2


def _stress_footprints(cfg: DagConfig, n: int, *, expanded: bool) -> tuple[int, int]:
    """Return ``(one_graph, total)`` with signed-i64 saturating arithmetic.

    For the authored preflight, total is ``n * one_graph``. A final already-expanded graph carries
    all step caps directly, while its per-copy control-plane floor still must be charged ``n``
    times.
    """
    from dagrun.sizing import (
        stress_control_floor_bytes,
        stress_copy_footprint_bytes,
    )

    footprint = stress_copy_footprint_bytes(cfg)
    if expanded:
        floor_total = min(2**63 - 1, stress_control_floor_bytes(cfg) * n)
        total = max(footprint, floor_total)
    else:
        total = min(2**63 - 1, footprint * n)
    return footprint, total


def _stress_memory_guard(
    cfg: DagConfig,
    n: int,
    *,
    expanded: bool,
    run_memory_budget_bytes: int | None = None,
) -> int:
    """Check stress memory either before expansion or after final planning.

    The early check receives one selected copy and multiplies its footprint by ``n``. The final
    check receives the ALREADY-EXPANDED graph after profile feedback / CPA allocation and therefore
    compares that graph's total directly -- multiplying by ``n`` again would double-count every
    copy. A missing box budget remains a loud warning for the early advisory preflight, but the
    final check fails closed because it is the last barrier before guest processes can spawn.
    """
    from dagrun.sizing import box_mem_budget_bytes

    footprint, total = _stress_footprints(cfg, n, expanded=expanded)
    if footprint >= 2**63 - 1 or total >= 2**63 - 1:
        subject = "final planned expanded-graph" if expanded else "per-copy"
        print(
            f"{PROG}: --stress {n}: REFUSED — {subject} memory footprint is unbounded or "
            "overflowed; declare finite positive per-step memory caps",
            file=sys.stderr,
        )
        return 2
    budget_candidates = [
        value
        for value in (box_mem_budget_bytes(), run_memory_budget_bytes)
        if value is not None
    ]
    budget = min(budget_candidates) if budget_candidates else None
    if budget is None:
        if expanded:
            print(
                f"{PROG}: --stress {n}: REFUSED — could not read the box memory budget for "
                "the final planned expanded graph; no step was started",
                file=sys.stderr,
            )
            return 2
        print(
            f"{PROG}: --stress {n}: WARNING could not read the box memory budget "
            f"(cgroup memory.max / MemAvailable); proceeding UNCHECKED with "
            f"{n} x {_human_bytes(footprint)}/copy = {_human_bytes(total)}. "
            "Watch for OOM.",
            file=sys.stderr,
        )
        return 0
    if expanded:
        if total > budget:
            print(
                f"{PROG}: --stress {n}: REFUSED — final planned expanded graph needs "
                f"{_human_bytes(total)}, exceeding the box memory budget "
                f"{_human_bytes(budget)}; no step was started",
                file=sys.stderr,
            )
            return 2
        return 0
    max_safe = budget // footprint
    if n > max_safe:
        print(
            f"{PROG}: --stress {n}: REFUSED — {n} parallel copies would exceed the box memory "
            "budget.\n"
            f"  requested copies:   {n}\n"
            f"  per-copy footprint: {_human_bytes(footprint)}\n"
            f"  total needed:       {_human_bytes(total)}\n"
            f"  box memory budget:  {_human_bytes(budget)} (min of cgroup memory.max + MemAvailable)\n"
            f"  max safe --stress:  {max_safe}\n"
            f"Re-run with --stress <= {max_safe} (cores are plentiful; memory is the binding "
            "constraint), or lower the per-copy footprint via a tighter per-step rss_baseline_bytes "
            "/ hard_mem_max_bytes hint.",
            file=sys.stderr,
        )
        return 2
    print(
        f"{PROG}: --stress {n}: OK — {n} x {_human_bytes(footprint)}/copy = "
        f"{_human_bytes(total)} fits the box memory budget {_human_bytes(budget)} "
        f"(max safe {max_safe}); actual concurrency will be measured from child-process "
        "lifetimes.",
        file=sys.stderr,
    )
    return 0


def _stress_guard(
    cfg: DagConfig, n: int, run_memory_budget_bytes: int | None = None
) -> int:
    """Early authored-hint preflight over one selected, CPU-capped graph copy."""
    return _stress_memory_guard(
        cfg, n, expanded=False, run_memory_budget_bytes=run_memory_budget_bytes
    )


def _final_stress_guard(
    cfg: DagConfig, n: int, run_memory_budget_bytes: int | None = None
) -> int:
    """Last no-spawn barrier over the already-expanded, finally planned graph."""
    return _stress_memory_guard(
        cfg, n, expanded=True, run_memory_budget_bytes=run_memory_budget_bytes
    )


def _print_stress_report(
    rows: Sequence[StepOutcome],
    n: int,
    max_concurrent_steps: int,
    max_steps: int,
    max_cpus: int,
    c: Palette,
) -> None:
    """Print the per-copy PASS/FAIL RATIO for a ``--stress`` run — the ratio IS the finding.

    Copies are grouped back to their original (base) step tag by stripping the ``#NN`` suffix;
    each group reports ``passed/total`` and names the copies that FAILED (and any that were
    ABORTED before finishing, so a collapsed ratio is never mistaken for all-pass)."""
    groups: dict[str, list[tuple[str, StepOutcome]]] = {}
    for outcome in rows:
        base, sep, idx = outcome.tag.rpartition("#")
        if not sep:
            base, idx = outcome.tag, ""
        groups.setdefault(base, []).append((idx, outcome))

    print(c.bold(f"stress results ({n} generated graph copies):"))
    for base in sorted(groups):
        items = groups[base]
        items.sort(key=lambda pair: pair[0])
        total = len(items)
        passed = sum(1 for _, o in items if o.ok)
        failed = [idx for idx, o in items if not o.ok and not o.aborted]
        aborted = [idx for idx, o in items if o.aborted]
        ratio = f"{passed}/{total} passed"
        line = c.green(ratio) if passed == total else c.red(ratio)
        detail = ""
        if failed:
            detail += " — " + c.red(
                f"{len(failed)} FAILED: " + ", ".join(f"#{i}" for i in failed)
            )
        if aborted:
            detail += " — " + c.yellow(
                f"{len(aborted)} aborted: " + ", ".join(f"#{i}" for i in aborted)
            )
        print(f"  {base}: {line}{detail}")
    print(
        f"  maximum concurrent steps: {max_concurrent_steps} "
        f"(--max-steps {max_steps}; --max-cpus {max_cpus} CPU target/per-step ceiling)"
    )


# --------------------------------------------------------------------------- table rendering
def _human_bytes(n: int | None) -> str:
    """Human-readable byte count (e.g. ``3.5 GiB``); ``"-"`` when unknown."""
    if n is None:
        return "-"
    value = float(n)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if value < 1024.0 or unit == "TiB":
            return f"{int(value)} B" if unit == "B" else f"{value:.1f} {unit}"
        value /= 1024.0
    return f"{n} B"


def _render_table(headers: Sequence[str], rows: Sequence[Sequence[str]], c: Palette) -> str:
    """Render a fixed-width table: the first column left-aligned, the rest right-aligned."""
    cols = len(headers)
    widths = [len(h) for h in headers]
    for row in rows:
        for i in range(cols):
            widths[i] = max(widths[i], len(row[i]))

    def fmt(cells: Sequence[str]) -> str:
        return "  ".join(
            f"{cells[i]:<{widths[i]}}" if i == 0 else f"{cells[i]:>{widths[i]}}"
            for i in range(cols)
        )

    lines = [c.bold(fmt(headers)), c.dim("  ".join("-" * w for w in widths))]
    lines.extend(fmt(row) for row in rows)
    return "\n".join(lines)


def _cell_secs(value: object) -> str:
    return f"{float(value):.3f}" if isinstance(value, (int, float)) else "-"


def _cell_secs_from_usec(value: object) -> str:
    return f"{value / 1e6:.3f}" if isinstance(value, int) else "-"


def _cell_bytes(value: object) -> str:
    return _human_bytes(value) if isinstance(value, int) else "-"


def _print_profile_table(rows: Sequence[Mapping[str, object]], c: Palette) -> None:
    """Print the per-step profile table (Feature C) to stdout.

    Columns: step | wall_s | user_s | sys_s | rss_hwm | oom | inner_jobs. ``user_s``/``sys_s``
    come from the per-step cgroup ``cpu.stat`` (present only under boxing) and ``rss_hwm`` from
    the step cgroup ``memory.peak``; each shows ``-`` when unavailable (an un-boxed run)."""
    if not rows:
        print(f"{PROG}: no per-step profile rows to show (nothing ran)", file=sys.stderr)
        return
    headers = ["step", "wall_s", "user_s", "sys_s", "rss_hwm", "oom", "inner_jobs"]
    table: list[list[str]] = []
    for row in rows:
        table.append(
            [
                str(row.get("step", "?")),
                _cell_secs(row.get("elapsed_s")),
                _cell_secs_from_usec(row.get("cpu.user_usec")),
                _cell_secs_from_usec(row.get("cpu.system_usec")),
                _cell_bytes(row.get("peak_bytes")),
                str(row.get("oom_kills", 0)),
                str(row.get("inner_jobs", "-")),
            ]
        )
    print(c.bold("per-step profile:"))
    print(_render_table(headers, table, c))


# --------------------------------------------------------------------------- sweep
_SWEEP_CAPTURE_FAILURE = 5  # internal only; _cmd_sweep maps it to the public failure status 1


@dataclasses.dataclass(frozen=True)
class _SweepMeasure:
    """One measured single-step run at a given inner-parallelism width."""

    wall_s: float
    user_s: float
    sys_s: float
    rss_hwm: int | None
    ok: bool
    returncode: int | None = None


@dataclasses.dataclass(frozen=True)
class _ProfilerRequest:
    """Validated profiler flags, before the profile-store path is resolved."""

    capture_perf: bool
    perf_window_s: float | None
    wprof_windows: int
    wprof_window_s: float
    sudo: tuple[str, ...]

    @property
    def enabled(self) -> bool:
        return self.capture_perf or self.wprof_windows > 0

    def config(self, output_dir: str) -> CaptureConfig:
        return CaptureConfig(
            output_dir=Path(output_dir),
            capture_perf=self.capture_perf,
            perf_window_s=self.perf_window_s,
            wprof_windows=self.wprof_windows,
            wprof_window_s=self.wprof_window_s,
            sudo=self.sudo,
        )


def _parse_profiler_duration(raw: str, option: str) -> float:
    try:
        seconds = parse_target_duration(raw)
    except ValueError:
        raise ValueError(
            f"invalid {option} {raw!r}: expected a positive duration with an optional "
            "ms/s/m/h suffix"
        ) from None
    if seconds <= 0.0:
        raise ValueError(f"invalid {option} {raw!r}: duration must be greater than zero")
    return seconds


def _parse_profiler_request(ns: argparse.Namespace) -> _ProfilerRequest:
    perf_value = getattr(ns, "perf_window", None)
    perf_raw = perf_value if isinstance(perf_value, str) else None
    perf_window_s = (
        None if perf_raw is None else _parse_profiler_duration(perf_raw, "--perf-window")
    )

    values = getattr(ns, "wprof_window", [])
    wprof_raws: list[str] = []
    if isinstance(values, list):
        for value in values:
            if not isinstance(value, str):
                raise ValueError("invalid --wprof-window value")
            wprof_raws.append(value)
    elif values:
        raise ValueError("invalid --wprof-window value")
    wprof_durations = [
        _parse_profiler_duration(raw, "--wprof-window") for raw in wprof_raws
    ]
    if wprof_durations and any(
        not math.isclose(value, wprof_durations[0], rel_tol=0.0, abs_tol=1e-12)
        for value in wprof_durations[1:]
    ):
        raise ValueError(
            "repeated --wprof-window durations must match; each occurrence requests another "
            "separate trial"
        )

    capture_perf = bool(getattr(ns, "perf_record", False)) or perf_window_s is not None
    profiler_sudo = bool(getattr(ns, "profiler_sudo", False))
    if profiler_sudo and not capture_perf and not wprof_durations:
        raise ValueError("--profiler-sudo requires --perf-record, --perf-window, or --wprof-window")
    return _ProfilerRequest(
        capture_perf=capture_perf,
        perf_window_s=perf_window_s,
        wprof_windows=len(wprof_durations),
        wprof_window_s=wprof_durations[0] if wprof_durations else 0.4,
        sudo=("sudo", "-n") if profiler_sudo else (),
    )


class _SweepMetricsSink:
    """Add sweep provenance to rows before delegating to the ordinary CSV sink."""

    def __init__(self, sink: MetricsSink, metadata: Mapping[str, object]) -> None:
        self._sink = sink
        self._metadata = dict(metadata)

    def start_run_window(self) -> RunWindow:
        return self._sink.start_run_window()

    def record_step_profiles(
        self, rows: Sequence[Mapping[str, object]], *, jobs: int
    ) -> str | None:
        enriched = [{**row, **self._metadata} for row in rows]
        return self._sink.record_step_profiles(enriched, jobs=jobs)

    def record_step_timeseries(
        self, rows: Sequence[Mapping[str, object]], *, jobs: int
    ) -> str | None:
        enriched = [{**row, **self._metadata} for row in rows]
        return self._sink.record_step_timeseries(enriched, jobs=jobs)


class _SweepCpuMetricsSink:
    """Fill unboxed sweep CPU columns from the same RUSAGE_CHILDREN window shown in the table.

    Boxed rows already carry cgroup-derived user/system seconds and retain those more precise
    values. The wrapper samples before delegating, so profile I/O itself is not charged to the
    child-process CPU measurement.
    """

    def __init__(self, sink: MetricsSink, started: resource.struct_rusage) -> None:
        self._sink = sink
        self._started = started

    def start_run_window(self) -> RunWindow:
        return self._sink.start_run_window()

    def record_step_profiles(
        self, rows: Sequence[Mapping[str, object]], *, jobs: int
    ) -> str | None:
        finished = resource.getrusage(resource.RUSAGE_CHILDREN)
        user_s = round(max(finished.ru_utime - self._started.ru_utime, 0.0), 3)
        sys_s = round(max(finished.ru_stime - self._started.ru_stime, 0.0), 3)
        enriched: list[dict[str, object]] = []
        for row in rows:
            copied = dict(row)
            if copied.get("user_s") in (None, ""):
                copied["user_s"] = user_s
            if copied.get("sys_s") in (None, ""):
                copied["sys_s"] = sys_s
            enriched.append(copied)
        return self._sink.record_step_profiles(enriched, jobs=jobs)

    def record_step_timeseries(
        self, rows: Sequence[Mapping[str, object]], *, jobs: int
    ) -> str | None:
        return self._sink.record_step_timeseries(rows, jobs=jobs)


def _parse_jobs_range(raw: str) -> tuple[int, int]:
    """Parse a sweep width range: ``"LO..HI"`` or a bare ``"N"`` (meaning ``1..N``).

    Returns ``(lo, hi)`` with ``1 <= lo <= hi``. Raises :class:`ValueError` on any malformed or
    out-of-order range so the caller can report a clear usage error."""
    text = raw.strip()
    if ".." in text:
        lo_s, _, hi_s = text.partition("..")
        try:
            lo, hi = int(lo_s), int(hi_s)
        except ValueError:
            # Clean, user-facing message matching the Rust build (never leak Python's raw
            # "invalid literal for int() with base 10: '...'").
            raise ValueError(f"invalid --jobs range {raw!r}: not an integer") from None
    else:
        try:
            hi = int(text)
        except ValueError:
            raise ValueError(f"invalid --jobs {raw!r}: not an integer") from None
        lo = 1
    if lo < 1 or hi < lo:
        raise ValueError(f"invalid --jobs range {raw!r}: need 1 <= LO <= HI")
    return lo, hi


def _run_single_step(
    base_step: Step,
    cfg: DagConfig,
    inner_jobs: int,
    cgroups: CgroupManager | None,
    metrics: MetricsSink | None,
    verbosity: int,
    *,
    vary_width: bool = True,
    profile_timeseries_interval_s: float | None = None,
) -> _SweepMeasure:
    """Run ONE step at a fixed inner-parallelism width and measure it.

    The step's ``preferred_inner_jobs`` is overridden to ``inner_jobs`` (so the width flows into
    the command via cmdtype, jobs_flag, and/or jobs_env) and its dependencies are cleared (a sweep runs the
    one step in isolation). CPU (user/sys) is measured from ``RUSAGE_CHILDREN`` deltas around the
    run; wall from the step's own recorded elapsed time; peak RSS from the step cgroup
    (``memory.peak``) when boxing is active."""
    step = dataclasses.replace(base_step, deps=[])
    if vary_width:
        step = dataclasses.replace(
            step,
            hint=dataclasses.replace(base_step.hint, preferred_inner_jobs=inner_jobs),
        )
    one = dataclasses.replace(cfg, steps=(step,))
    ru0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    persisted_metrics = _SweepCpuMetricsSink(metrics, ru0) if metrics is not None else None
    wall_start = time.monotonic()
    result = run_dag_limited(
        one,
        max_steps=1,
        max_cpus=inner_jobs,
        cgroups=cgroups,
        metrics=persisted_metrics,
        keep_going=False,
        verbosity=verbosity,
        profile_timeseries_interval_s=profile_timeseries_interval_s,
    )
    wall_measured = time.monotonic() - wall_start
    ru1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    user_s = max(ru1.ru_utime - ru0.ru_utime, 0.0)
    sys_s = max(ru1.ru_stime - ru0.ru_stime, 0.0)
    wall_s = wall_measured
    rss: int | None = None
    if result.step_profile_rows:
        row = result.step_profile_rows[0]
        elapsed = row.get("elapsed_s")
        if isinstance(elapsed, (int, float)) and not isinstance(elapsed, bool):
            wall_s = float(elapsed)
        row_user = row.get("user_s")
        if isinstance(row_user, (int, float)) and not isinstance(row_user, bool):
            user_s = float(row_user)
        row_sys = row.get("sys_s")
        if isinstance(row_sys, (int, float)) and not isinstance(row_sys, bool):
            sys_s = float(row_sys)
        peak = row.get("peak_bytes")
        if isinstance(peak, int) and not isinstance(peak, bool):
            rss = peak
    returncode = result.outcomes[0].returncode if len(result.outcomes) == 1 else None
    return _SweepMeasure(
        wall_s=wall_s,
        user_s=user_s,
        sys_s=sys_s,
        rss_hwm=rss,
        ok=result.ok,
        returncode=returncode,
    )


def _cmd_pin_run(ns: argparse.Namespace) -> int:
    """Reserve K collision-free cores, box the command's whole subtree onto them,
    run it, and RELEASE on exit (normal or failure).

    This is the consumer wrapper a same-core benchmark harness wants: it never
    picks a core itself and never collides with a concurrent reservation, so two
    benchmarks running at once land on DISJOINT cores. The reservation is held
    for exactly the command's lifetime; a crash is reclaimed by the next
    acquire's dead-holder sweep."""
    from dagrun import cpuset_allocator as _cpuset
    from dagrun import reservation as _res

    cmd = list(ns.cmd or [])
    # argparse.REMAINDER keeps a leading '--'; drop it so 'pin-run --cores 1 -- echo hi' works.
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]
    if not cmd:
        print(f"{PROG}: pin-run: no command given (use '-- CMD [ARGS...]')", file=sys.stderr)
        return 2

    k = int(ns.cores)
    tag = str(ns.tag or "pin-run")
    try:
        reservation = _res.acquire(k, tag=tag)
    except (ValueError, _res.InsufficientCoresError) as exc:
        print(f"{PROG}: pin-run: {exc}", file=sys.stderr)
        return 3
    try:
        return _cpuset._run_reserved_hard(
            reservation.cores,
            cmd,
            tag=tag,
            prog=f"{PROG}: pin-run",
        )
    finally:
        reservation.release()


def _effective_sweep_topology(max_cpus: int | None = None) -> CpuTopology:
    """Topology available to a sweep, bounded by the effective cgroup CPU budget."""

    topology = limit_topology(detect_cpu_topology(), container_core_budget())
    if max_cpus is not None:
        topology = limit_topology(topology, max_cpus)
    if topology.physical_core_count is None:
        # Match the Rust/runtime policy: when sysfs cannot prove an SMT boundary, use the logical
        # width rather than inventing a ratio. The pure discovery API retains ``None`` so callers
        # can still distinguish unknown topology from a measured one.
        return CpuTopology(
            allowed_logical_cpus=topology.allowed_logical_cpus,
            physical_core_count=topology.logical_thread_count,
        )
    return topology


def _sweep_memory_error(
    step: Step,
    cfg: DagConfig,
    width: int,
    inherited_max_mem_bytes: int | None,
) -> str | None:
    """Explain why one sweep width cannot fit its delegated parent, if bounded."""
    if inherited_max_mem_bytes is None:
        return None
    cap = _step_mem_cap_for_inner_jobs(
        step,
        width,
        mem_cap_factor=cfg.mem_cap_factor,
        default_cap_bytes=cfg.default_step_mem_cap_bytes,
    )
    if cap is not None and cap <= inherited_max_mem_bytes:
        return None
    modeled = "unbounded" if cap is None else f"{cap} bytes"
    return (
        f"step {step.tag!r} at width {width} has modeled memory cap {modeled}, exceeding "
        f"inherited delegated memory ceiling {inherited_max_mem_bytes} bytes"
    )


def _sweep_cpu_error(max_width: int, inherited_max_cpus: int | None) -> str | None:
    if inherited_max_cpus is None or max_width <= inherited_max_cpus:
        return None
    return (
        f"--jobs reaches {max_width}, exceeding inherited delegated CPU ceiling "
        f"{inherited_max_cpus}; refusing physically throttled measurements"
    )


def _fixed_step_width(step: Step, cfg: DagConfig, topology: CpuTopology) -> int:
    """Best honest width label for a step whose guest parallelism cannot be rewritten."""

    return effective_cpu_count(step, cfg.default_step_cpu_count) or topology.logical_thread_count


def _sweep_sink(
    perf_dir: str | None,
    git_sha: str,
    metadata: Mapping[str, object],
    cgroups: CgroupManager | None,
) -> MetricsSink | None:
    """Create a fresh per-execution sink carrying graph-sweep provenance."""

    if perf_dir is None:
        return None
    from dagrun.perflog import CsvMetricsSink

    sink = CsvMetricsSink(
        perf_dir,
        git_sha=git_sha,
        enforcement_kind="cgroup-v2" if cgroups is not None else "unboxed",
        runner_name="sweep",
    )
    return sink if not metadata else _SweepMetricsSink(sink, metadata)


def _current_sweep_selection(
    step: Step,
    cfg: DagConfig,
    samples_by_width: Mapping[int, Sequence[_SweepMeasure]],
    git_sha: str,
) -> SweetSpotSelection:
    """Fit a capture width from only this invocation's uninstrumented observations."""

    digest = workload_digest(step, cfg.default_jobs_flag, cfg.default_jobs_env)
    buckets = {
        (step.tag, width): tuple(
            Sample(
                elapsed_s=sample.wall_s,
                contention=0.0,
                cpu_s=sample.user_s + sample.sys_s,
                effective_cores=(
                    (sample.user_s + sample.sys_s) / sample.wall_s
                    if sample.wall_s > 0.0
                    else None
                ),
                throttled_s=None,
                peak_bytes=sample.rss_hwm,
                workload_digest=digest,
            )
            for sample in width_samples
        )
        for width, width_samples in samples_by_width.items()
        if width_samples
    }
    core_budget = max((width for _, width in buckets), default=0)
    models = step_speedups_from_buckets(
        buckets, core_budget=core_budget if core_budget > 0 else None
    )
    model = models.get(step.tag)
    if model is not None:
        return select_sweet_spot(model, workload_digest=digest, git_sha=git_sha)

    # The model intentionally requires two distinct widths. A one-point explicit sweep can still
    # be profiled honestly: its only measured width is the only evidence-backed choice, but name
    # that fallback rather than pretending a plateau was fitted.
    if len(buckets) == 1:
        (_, width), measurements = next(iter(buckets.items()))
        walls = [sample.elapsed_s for sample in measurements if sample.elapsed_s is not None]
        if not walls:
            raise ValueError(f"step {step.tag!r} has no positive wall measurement")
        wall = statistics.median(walls)
        if wall <= 0.0:
            raise ValueError(f"step {step.tag!r} has no positive wall measurement")
        return SweetSpotSelection(
            step=step.tag,
            workload_digest=digest,
            inner_jobs=width,
            expected_wall_s=wall,
            baseline_inner_jobs=width,
            speedup=1.0,
            model_wall_s=wall,
            raw_wall_s=wall,
            source="single-measured-width",
            git_sha=git_sha,
        )
    raise ValueError(f"step {step.tag!r} has insufficient valid sweep data for profiler capture")


def _profiled_step(base_step: Step, cfg: DagConfig, request: IsolatedTrialRequest) -> Step:
    """Wrap one width-resolved guest with its profiler and exact launch handshake.

    The handshake lives inside perf's command, after its ``--`` separator, rather than around the
    outer perf/sudo wrapper.  It therefore timestamps the actual guest after scheduler/cgroup and
    profiler setup, then blocks until the capture controller has anchored its monotonic clock.
    """

    guest_command = command_with_inner_jobs(
        base_step, cfg.default_jobs_flag, request.inner_jobs
    )
    inner_env = dict(request.env)
    jobs_env = env_with_inner_jobs(base_step, cfg.default_jobs_env, request.inner_jobs)
    for name, value in jobs_env.items():
        inner_env.pop(name, None)
        guest_command = _checked_jobs_env_export(name, value) + guest_command
    inner_env.update(cmdtype_env_with_inner_jobs(base_step, request.inner_jobs))
    ready_path = shlex.quote(str(request.guest_launch_ready_path))
    release_path = shlex.quote(str(request.guest_launch_release_path))
    guest_command = (
        f"if ! printf '%s\\n' \"$$\" > {ready_path}; then\n"
        "  echo 'dagrun: profiler guest-launch notification failed' >&2\n"
        "  exit 125\n"
        "fi\n"
        f"if ! IFS= read -r _ < {release_path}; then\n"
        "  echo 'dagrun: profiler guest-launch release failed' >&2\n"
        "  exit 125\n"
        "fi\n"
        f"{guest_command}"
    )
    argv = list(request.argv_prefix)
    if inner_env or jobs_env:
        argv.append("env")
        # The profiler wrapper opts out of scheduler injection. Its actual guest Bash must
        # also start without the channel, including an inherited or request-provided value.
        for name in jobs_env:
            argv.extend(("-u", name))
        argv.extend(f"{name}={value}" for name, value in sorted(inner_env.items()))
    argv.extend(("bash", "-c", guest_command))
    # The guest width is already rendered inside the profiler wrapper. Neutralize scheduler-side
    # argv/env injection so the profiler, rather than the guest, cannot accidentally receive a
    # second jobs flag at the end of the wrapper command.
    return dataclasses.replace(
        base_step,
        cmd=shlex.join(argv),
        cmdtype=CmdType.UNKNOWN,
        jobs_flag="",
        jobs_env="",
    )


def _run_profiler_captures(
    cfg: DagConfig,
    step_samples: Sequence[tuple[Step, Mapping[int, Sequence[_SweepMeasure]]]],
    capture_config: CaptureConfig,
    cgroups: CgroupManager | None,
    sweep_git_sha: str,
    verbosity: int,
) -> int:
    """Run one post-sweep capture session per measured step, never persisting its trials."""

    for step, samples_by_width in step_samples:
        try:
            selection = _current_sweep_selection(
                step, cfg, samples_by_width, sweep_git_sha
            )
        except ValueError as exc:
            print(f"{PROG}: sweep: profiler capture selection failed: {exc}", file=sys.stderr)
            return _SWEEP_CAPTURE_FAILURE
        print(
            f"sweep: {step.tag}: profiler capture at sweet-spot width "
            f"{selection.inner_jobs} (expected wall {selection.expected_wall_s:.3f}s)"
        )

        def run_trial(request: IsolatedTrialRequest) -> IsolatedTrialResult:
            if request.step != step.tag or request.inner_jobs != selection.inner_jobs:
                raise ValueError("profiler callback received a mismatched step or width")
            instrumented_step = _profiled_step(step, cfg, request)
            measure = _run_single_step(
                instrumented_step,
                cfg,
                request.inner_jobs,
                cgroups,
                None,
                verbosity,
                profile_timeseries_interval_s=None,
            )
            return IsolatedTrialResult(
                returncode=measure.returncode,
                wall_s=measure.wall_s,
                detail="" if measure.ok else "instrumented step failed",
            )

        try:
            manifest = capture_at_sweet_spot(selection, capture_config, run_trial)
        except ProfileCaptureError as exc:
            print(f"{PROG}: sweep: profiler capture FAILED: {exc}", file=sys.stderr)
            return _SWEEP_CAPTURE_FAILURE
        except (OSError, ValueError) as exc:
            print(f"{PROG}: sweep: profiler capture FAILED: {exc}", file=sys.stderr)
            return _SWEEP_CAPTURE_FAILURE
        print(f"sweep: {step.tag}: profiler capture manifest: {manifest.path}")
    return 0


def _run_legacy_sweep(
    cfg: DagConfig,
    step_tag: str,
    jobs_spec: str,
    repeat: int,
    cgroups: CgroupManager | None,
    perf_dir: str | None,
    sweep_git_sha: str,
    verbosity: int,
    c: Palette,
    profile_timeseries_interval_s: float | None = None,
    capture_config: CaptureConfig | None = None,
    inherited_max_cpus: int | None = None,
    inherited_max_mem_bytes: int | None = None,
) -> int:
    """The original one-step dense-range command, kept byte-compatible in normal output."""

    by_tag = cfg.by_tag()
    if step_tag not in by_tag:
        known = ", ".join(sorted(by_tag)) or "(none)"
        print(
            f"{PROG}: sweep: unknown --step tag {step_tag!r}. Known tags: {known}",
            file=sys.stderr,
        )
        return 2
    try:
        lo, hi = _parse_jobs_range(jobs_spec)
    except ValueError as exc:
        print(f"{PROG}: sweep: {exc}", file=sys.stderr)
        return 2
    if error := _sweep_cpu_error(hi, inherited_max_cpus):
        print(f"{PROG}: sweep: {error}", file=sys.stderr)
        return 2
    base_step = by_tag[step_tag]
    if not step_width_is_resizable(
        base_step, cfg.default_jobs_flag, cfg.default_jobs_env
    ):
        print(
            f"{PROG}: sweep: step {step_tag!r} offers no width channel, so --jobs cannot "
            "change guest parallelism; set jobs_flag to the guest's worker-count option or "
            "jobs_env/default_jobs_env to its worker-count environment variable",
            file=sys.stderr,
        )
        return 2
    if error := _sweep_memory_error(
        base_step, cfg, hi, inherited_max_mem_bytes
    ):
        print(f"{PROG}: sweep: {error}; refusing", file=sys.stderr)
        return 2

    measures: list[tuple[int, _SweepMeasure]] = []
    samples_by_width: dict[int, list[_SweepMeasure]] = {}
    for jobs in range(lo, hi + 1):
        best: _SweepMeasure | None = None
        for _ in range(repeat):
            m = _run_single_step(
                base_step,
                cfg,
                jobs,
                cgroups,
                _sweep_sink(perf_dir, sweep_git_sha, {}, cgroups),
                verbosity,
                profile_timeseries_interval_s=profile_timeseries_interval_s,
            )
            if not m.ok:
                print(
                    f"{PROG}: sweep: step {step_tag!r} FAILED at -j{jobs}; aborting the sweep",
                    file=sys.stderr,
                )
                return 1
            samples_by_width.setdefault(jobs, []).append(m)
            if best is None or m.wall_s < best.wall_s:
                best = m
        assert best is not None
        measures.append((jobs, best))

    _print_sweep_table(step_tag, lo, measures, c)
    if capture_config is not None:
        return _run_profiler_captures(
            cfg,
            ((base_step, samples_by_width),),
            capture_config,
            cgroups,
            sweep_git_sha,
            verbosity,
        )
    return 0


def _print_fixed_sweep_measure(step_tag: str, measure: _SweepMeasure, c: Palette) -> None:
    """Print the one-row table used for a node whose guest width cannot be rewritten."""

    headers = ["jobs", "wall_s", "user_s", "sys_s", "rss_hwm", "speedup"]
    table = [
        [
            "fixed",
            f"{measure.wall_s:.3f}",
            f"{measure.user_s:.3f}",
            f"{measure.sys_s:.3f}",
            _human_bytes(measure.rss_hwm),
            "-",
        ]
    ]
    print(c.bold(f"parallel-speedup sweep: {step_tag}"))
    print(_render_table(headers, table, c))


def _run_target_sweep(
    cfg: DagConfig,
    *,
    step_tag: str | None,
    jobs_spec: str | None,
    target_s: float,
    repeat: int,
    cgroups: CgroupManager | None,
    perf_dir: str | None,
    sweep_git_sha: str,
    verbosity: int,
    c: Palette,
    profile_timeseries_interval_s: float | None = None,
    capture_config: CaptureConfig | None = None,
    inherited_max_cpus: int | None = None,
    inherited_max_mem_bytes: int | None = None,
) -> int:
    """Run a graph-wide, soft-target, cumulative multi-pass scaling sweep."""

    try:
        ordered = stable_topological_steps(cfg.steps)
    except ValueError as exc:
        print(f"{PROG}: sweep: {exc}", file=sys.stderr)
        return 2
    by_tag = cfg.by_tag()
    if step_tag is not None:
        if step_tag not in by_tag:
            known = ", ".join(sorted(by_tag)) or "(none)"
            print(
                f"{PROG}: sweep: unknown --step tag {step_tag!r}. Known tags: {known}",
                file=sys.stderr,
            )
            return 2
        ordered = (by_tag[step_tag],)

    topology = (
        _effective_sweep_topology()
        if inherited_max_cpus is None
        else _effective_sweep_topology(inherited_max_cpus)
    )
    physical_cores = topology.physical_core_count or topology.logical_thread_count
    if jobs_spec is None:
        initial_points = labeled_width_grid_for_pass(topology, 1)
        initial_widths = tuple(point.inner_jobs for point in initial_points)
        initial_sources = {point.inner_jobs: point.source_label for point in initial_points}
    else:
        try:
            initial_widths = parse_widths(jobs_spec)
        except ValueError as exc:
            print(f"{PROG}: sweep: {exc}", file=sys.stderr)
            return 2
        initial_sources = {width: "explicit" for width in initial_widths}
    if error := _sweep_cpu_error(max(initial_widths, default=1), inherited_max_cpus):
        print(f"{PROG}: sweep: {error}", file=sys.stderr)
        return 2

    max_width = max(initial_widths, default=1)
    for step in ordered:
        if step.skip_reason is not None:
            # Intentional skips never execute, so their authored resource hints do not consume
            # or violate the inherited envelope. Keep preflight aligned with the run loop below.
            continue
        resizable = step_width_is_resizable(
            step, cfg.default_jobs_flag, cfg.default_jobs_env
        )
        width = max_width if resizable else _fixed_step_width(step, cfg, topology)
        if (
            step_tag is not None
            and not resizable
            and width > topology.logical_thread_count
        ):
            print(
                f"{PROG}: sweep: selected fixed-width step {step.tag!r} requests {width} "
                f"CPUs, exceeding the effective {topology.logical_thread_count}-CPU budget; "
                "refusing an empty successful sweep",
                file=sys.stderr,
            )
            return 2
        if error := _sweep_memory_error(step, cfg, width, inherited_max_mem_bytes):
            print(f"{PROG}: sweep: {error}; refusing", file=sys.stderr)
            return 2

    from dagrun.perflog import new_run_id

    sweep_id = new_run_id()
    sample_number = 0
    has_resizable = any(
        step.skip_reason is None
        and step_width_is_resizable(step, cfg.default_jobs_flag, cfg.default_jobs_env)
        for step in ordered
    )
    print(
        c.bold(
            f"parallel-scaling sweep: {len(ordered)} node(s), target {target_s:.3f}s, "
            f"topology {physical_cores}/{topology.logical_thread_count} physical/logical"
        )
    )

    sweep_started = time.monotonic()
    pass_number = 1
    pass_one_samples: dict[str, dict[int, list[_SweepMeasure]]] = {}
    skipped_fixed: list[str] = []
    while True:
        grid = width_grid_for_pass(initial_widths, pass_number)
        grid_text = ",".join(str(width) for width in grid)
        print(
            f"sweep pass {pass_number} starting: cumulative widths [{grid_text}]"
            + (" (mandatory minimum pass)" if pass_number == 1 else "")
        )
        pass_started = time.monotonic()

        for step in ordered:
            if step.skip_reason is not None:
                if pass_number == 1:
                    print(
                        f"sweep: {step.tag}: intentionally skipped by the DAG; "
                        "no process was started"
                    )
                continue
            resizable = step_width_is_resizable(
                step, cfg.default_jobs_flag, cfg.default_jobs_env
            )
            if not resizable:
                if pass_number != 1:
                    continue
                fixed_width = _fixed_step_width(step, cfg, topology)
                if fixed_width > topology.logical_thread_count:
                    print(
                        f"sweep: {step.tag}: fixed configured width {fixed_width} exceeds "
                        f"the effective {topology.logical_thread_count}-CPU budget; skipping"
                    )
                    skipped_fixed.append(step.tag)
                    continue
                print(
                    f"sweep: {step.tag}: no jobs_flag/jobs_env width channel; characterizing "
                    f"its configured width once (width {fixed_width})"
                )
                sample_number += 1
                metadata: dict[str, object] = {
                    "sweep_mode": "target-time",
                    "sweep_id": sweep_id,
                    "sweep_pass": pass_number,
                    "sweep_sample": sample_number,
                    "sweep_repeat": 1,
                    "sweep_width_source": "fixed",
                    "workload_digest": workload_digest(
                        step, cfg.default_jobs_flag, cfg.default_jobs_env
                    ),
                    "sweep_target_s": f"{target_s:.6f}",
                    "sweep_physical_cores": physical_cores,
                    "sweep_logical_cpus": topology.logical_thread_count,
                }
                measure = _run_single_step(
                    step,
                    cfg,
                    fixed_width,
                    cgroups,
                    _sweep_sink(perf_dir, sweep_git_sha, metadata, cgroups),
                    verbosity,
                    vary_width=False,
                    profile_timeseries_interval_s=profile_timeseries_interval_s,
                )
                if not measure.ok:
                    print(
                        f"{PROG}: sweep: step {step.tag!r} FAILED at its configured width; "
                        "aborting the sweep",
                        file=sys.stderr,
                    )
                    return 1
                if pass_number == 1:
                    pass_one_samples.setdefault(step.tag, {}).setdefault(
                        fixed_width, []
                    ).append(measure)
                _print_fixed_sweep_measure(step.tag, measure, c)
                continue

            measures: list[tuple[int, _SweepMeasure]] = []
            for jobs in grid:
                best: _SweepMeasure | None = None
                width_source = initial_sources.get(jobs, "midpoint")
                for repeat_number in range(1, repeat + 1):
                    sample_number += 1
                    metadata = {
                        "sweep_mode": "target-time",
                        "sweep_id": sweep_id,
                        "sweep_pass": pass_number,
                        "sweep_sample": sample_number,
                        "sweep_repeat": repeat_number,
                        "sweep_width_source": width_source,
                        "workload_digest": workload_digest(
                            step, cfg.default_jobs_flag, cfg.default_jobs_env
                        ),
                        "sweep_target_s": f"{target_s:.6f}",
                        "sweep_physical_cores": physical_cores,
                        "sweep_logical_cpus": topology.logical_thread_count,
                    }
                    measure = _run_single_step(
                        step,
                        cfg,
                        jobs,
                        cgroups,
                        _sweep_sink(perf_dir, sweep_git_sha, metadata, cgroups),
                        verbosity,
                        profile_timeseries_interval_s=profile_timeseries_interval_s,
                    )
                    if not measure.ok:
                        print(
                            f"{PROG}: sweep: step {step.tag!r} FAILED at -j{jobs} in pass "
                            f"{pass_number}; aborting the sweep",
                            file=sys.stderr,
                        )
                        return 1
                    if pass_number == 1:
                        pass_one_samples.setdefault(step.tag, {}).setdefault(jobs, []).append(
                            measure
                        )
                    if best is None or measure.wall_s < best.wall_s:
                        best = measure
                assert best is not None
                measures.append((jobs, best))
            _print_sweep_table(step.tag, grid[0], measures, c)

        if pass_number == 1 and capture_config is not None:
            capture_steps = tuple(
                (step, pass_one_samples[step.tag])
                for step in ordered
                if step.tag in pass_one_samples
            )
            capture_code = _run_profiler_captures(
                cfg,
                capture_steps,
                capture_config,
                cgroups,
                sweep_git_sha,
                verbosity,
            )
            if capture_code != 0:
                return capture_code

        pass_elapsed = time.monotonic() - pass_started
        total_elapsed = time.monotonic() - sweep_started
        print(
            f"sweep pass {pass_number} complete in {pass_elapsed:.3f}s; total elapsed "
            f"{total_elapsed:.3f}s / target {target_s:.3f}s"
        )
        if pass_number == 1:
            print(
                f"minimum pass elapsed {pass_elapsed:.3f}s; target {target_s:.3f}s; "
                f"overrun {max(0.0, pass_elapsed - target_s):.3f}s"
            )

        if total_elapsed >= target_s or not has_resizable:
            completed_passes = pass_number
            break
        pass_number += 1

    elapsed = time.monotonic() - sweep_started
    print(
        f"target-time sweep complete: {completed_passes} pass(es), {sample_number} sample(s), "
        f"elapsed {elapsed:.3f}s, target {target_s:.3f}s, "
        f"overrun {max(0.0, elapsed - target_s):.3f}s"
    )
    if skipped_fixed:
        print(
            "sweep: fixed nodes skipped without samples because their configured widths exceed "
            f"the effective CPU budget: {', '.join(skipped_fixed)}"
        )
    elif not has_resizable:
        print(
            "sweep: no runnable selected node has a jobs_flag/jobs_env width channel; "
            "fixed nodes were handled once"
        )
    return 0


def _cmd_sweep(ns: argparse.Namespace, c: Palette) -> int:
    """Parallel-speedup sweep: legacy one-step mode or target-time graph mode."""
    dag_arg = ns.dag if isinstance(ns.dag, str) else None
    if dag_arg is None:
        print(f"{PROG}: sweep: --dag FILE is required", file=sys.stderr)
        return 2
    target_raw = ns.target_time if isinstance(ns.target_time, str) else None
    step_tag = ns.step if isinstance(ns.step, str) else None
    jobs_spec = ns.jobs if isinstance(ns.jobs, str) else None
    output_dir_arg = ns.output_dir if isinstance(ns.output_dir, str) else None
    legacy_perf_dir_arg = ns.perf_dir if isinstance(ns.perf_dir, str) else None
    report_html = ns.report_html if isinstance(ns.report_html, str) else None
    no_report = bool(ns.no_report)
    try:
        profiler_request = _parse_profiler_request(ns)
    except ValueError as exc:
        print(f"{PROG}: sweep: {exc}", file=sys.stderr)
        return 2
    if output_dir_arg is not None and not output_dir_arg.strip():
        print(f"{PROG}: sweep: --output-dir must not be empty", file=sys.stderr)
        return 2
    if legacy_perf_dir_arg is not None and not legacy_perf_dir_arg.strip():
        print(f"{PROG}: sweep: --perf-dir must not be empty", file=sys.stderr)
        return 2
    if (
        output_dir_arg is not None
        and legacy_perf_dir_arg is not None
        and output_dir_arg != legacy_perf_dir_arg
    ):
        print(
            f"{PROG}: sweep: --output-dir and --perf-dir disagree",
            file=sys.stderr,
        )
        return 2
    if report_html is not None and not report_html.strip():
        print(f"{PROG}: sweep: --report-html must not be empty", file=sys.stderr)
        return 2
    if report_html is not None and bool(ns.no_profile):
        print(
            f"{PROG}: sweep: --report-html cannot be combined with --no-profile",
            file=sys.stderr,
        )
        return 2
    if report_html is not None and no_report:
        print(
            f"{PROG}: sweep: --report-html cannot be combined with --no-report",
            file=sys.stderr,
        )
        return 2
    if profiler_request.enabled and bool(ns.no_profile):
        print(
            f"{PROG}: sweep: profiler capture options cannot be combined with --no-profile; "
            "captures need an output directory",
            file=sys.stderr,
        )
        return 2
    timeseries_value = getattr(ns, "profile_timeseries", None)
    timeseries_raw = timeseries_value if isinstance(timeseries_value, str) else None
    if timeseries_raw is not None and bool(ns.no_profile):
        print(
            f"{PROG}: sweep: --profile-timeseries cannot be combined with --no-profile",
            file=sys.stderr,
        )
        return 2
    profile_timeseries_interval_s: float | None = None
    if timeseries_raw is not None:
        try:
            profile_timeseries_interval_s = _parse_profile_timeseries_duration(timeseries_raw)
        except ValueError as exc:
            print(f"{PROG}: sweep: {exc}", file=sys.stderr)
            return 2
    if target_raw is None and (step_tag is None or jobs_spec is None):
        print(
            f"{PROG}: sweep: without --target-time, --step and --jobs are required",
            file=sys.stderr,
        )
        return 2
    target_s: float | None = None
    if target_raw is not None:
        try:
            target_s = parse_target_duration(target_raw)
        except ValueError as exc:
            print(f"{PROG}: sweep: {exc}", file=sys.stderr)
            return 2
    try:
        cfg = _load(dag_arg)
    except (OSError, DagJsonError) as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2
    repeat = max(1, int(ns.repeat))
    inherited_max_cpus, inherited_max_mem_bytes, inherited_error = (
        _inherited_delegated_limits()
    )
    if inherited_error is not None:
        print(f"{PROG}: ERROR: {inherited_error}.", file=sys.stderr)
        return 3

    # Reject every usage/configuration error before cgroup setup or profile reporting. Besides
    # matching the Rust CLI, this prevents an invalid invocation from claiming old store files as
    # outputs of a sweep that never started.
    if target_s is None:
        assert step_tag is not None and jobs_spec is not None
        try:
            _parse_jobs_range(jobs_spec)
        except ValueError as exc:
            print(f"{PROG}: sweep: {exc}", file=sys.stderr)
            return 2
        by_tag = cfg.by_tag()
        if step_tag not in by_tag:
            known = ", ".join(sorted(by_tag)) or "(none)"
            print(
                f"{PROG}: sweep: unknown --step tag {step_tag!r}. Known tags: {known}",
                file=sys.stderr,
            )
            return 2
        if not step_width_is_resizable(
            by_tag[step_tag], cfg.default_jobs_flag, cfg.default_jobs_env
        ):
            print(
                f"{PROG}: sweep: step {step_tag!r} offers no width channel, so --jobs cannot "
                "change guest parallelism; set jobs_flag to the guest's worker-count option or "
                "jobs_env/default_jobs_env to its worker-count environment variable",
                file=sys.stderr,
            )
            return 2
    else:
        try:
            stable_topological_steps(cfg.steps)
            if jobs_spec is not None:
                parse_widths(jobs_spec)
        except ValueError as exc:
            print(f"{PROG}: sweep: {exc}", file=sys.stderr)
            return 2
        if step_tag is not None and step_tag not in cfg.by_tag():
            known = ", ".join(sorted(cfg.by_tag())) or "(none)"
            print(
                f"{PROG}: sweep: unknown --step tag {step_tag!r}. Known tags: {known}",
                file=sys.stderr,
            )
            return 2

    # Cgroup boxing is ON by default here too (so the sweep measures under real boxing).
    cgroups, code = _resolve_cgroup_manager(
        bool(ns.allow_cgroup_failure),
        bool(getattr(ns, "unsafe_no_cgroups", False)),
        max_cpus=inherited_max_cpus,
        max_mem_bytes=inherited_max_mem_bytes,
    )
    if code != 0:
        return code
    if profile_timeseries_interval_s is not None and cgroups is None:
        print(
            f"{PROG}: sweep: --profile-timeseries requires active cgroup-v2 containment; "
            "no step was started",
            file=sys.stderr,
        )
        return 3

    explicit_profile_dir = output_dir_arg or legacy_perf_dir_arg
    perf_dir, source = _resolve_profile_dir(explicit_profile_dir, bool(ns.no_profile))
    if output_dir_arg is not None:
        source = "--output-dir"
    sweep_git_sha = _git_sha() if perf_dir is not None else ""
    capture_config = (
        profiler_request.config(perf_dir)
        if profiler_request.enabled and perf_dir is not None
        else None
    )
    verbosity = int(ns.verbosity)
    if target_s is None:
        assert step_tag is not None and jobs_spec is not None
        code = _run_legacy_sweep(
            cfg,
            step_tag,
            jobs_spec,
            repeat,
            cgroups,
            perf_dir,
            sweep_git_sha,
            verbosity,
            c,
            profile_timeseries_interval_s,
            capture_config,
            inherited_max_cpus,
            inherited_max_mem_bytes,
        )
    else:
        code = _run_target_sweep(
            cfg,
            step_tag=step_tag,
            jobs_spec=jobs_spec,
            target_s=target_s,
            repeat=repeat,
            cgroups=cgroups,
            perf_dir=perf_dir,
            sweep_git_sha=sweep_git_sha,
            verbosity=verbosity,
            c=c,
            profile_timeseries_interval_s=profile_timeseries_interval_s,
            capture_config=capture_config,
            inherited_max_cpus=inherited_max_cpus,
            inherited_max_mem_bytes=inherited_max_mem_bytes,
        )
    capture_failed = code == _SWEEP_CAPTURE_FAILURE
    explicit_report_failed = False
    if perf_dir is not None:
        _report_profile_written(perf_dir, source)
        if code == 0 or capture_failed:
            _refresh_scaling_model(perf_dir, cfg)
            if not no_report:
                report_ok = _refresh_profile_report(perf_dir, cfg, dag_arg, report_html)
                explicit_report_failed = report_html is not None and not report_ok
    return 1 if capture_failed or explicit_report_failed else code


def _print_sweep_table(
    step_tag: str, baseline_jobs: int, measures: Sequence[tuple[int, _SweepMeasure]], c: Palette
) -> None:
    """Print the parallel-speedup sweep table (Feature B) to stdout."""
    baseline_wall = measures[0][1].wall_s if measures else 0.0
    speedup_col = f"speedup(vs j{baseline_jobs})"
    headers = ["jobs", "wall_s", "user_s", "sys_s", "rss_hwm", speedup_col]
    table: list[list[str]] = []
    for jobs, m in measures:
        speedup = f"{baseline_wall / m.wall_s:.2f}x" if m.wall_s > 0 else "-"
        table.append(
            [
                str(jobs),
                f"{m.wall_s:.3f}",
                f"{m.user_s:.3f}",
                f"{m.sys_s:.3f}",
                _human_bytes(m.rss_hwm),
                speedup,
            ]
        )
    print(c.bold(f"parallel-speedup sweep: {step_tag}"))
    print(_render_table(headers, table, c))


def _inherited_delegated_limits() -> tuple[int | None, int | None, str | None]:
    """Return validated nested ``(CPU, memory, error)`` limits before planning starts."""
    from dagrun import cgroup as cg

    root, error = cg.delegated_cgroup_root()
    if error is not None:
        return None, None, f"invalid ${cg.DELEGATED_CGROUP_ENV}: {error}"
    if root is None:
        return None, None, None
    limits = cg.delegated_resource_limits(root)
    unknown: list[str] = []
    if not limits.cpu_known:
        unknown.append("cpu.max")
    if not limits.memory_known:
        unknown.append("memory.max")
    if unknown:
        return (
            None,
            None,
            f"delegated parent resource envelope has unreadable/malformed {', '.join(unknown)}",
        )
    return limits.max_cpus, limits.memory_max_bytes, None


def _select_max_cpus(
    ns: argparse.Namespace, inherited_max_cpus: int | None = None
) -> int:
    """Resolve the maximum total core-equivalents for one run.

    The inherited cgroup quota, process affinity, and shared aggregate-slice budget jointly bound
    the default. An opt-in hard ``--cores`` reservation can only tighten either the default or an
    explicit ``--max-cpus`` request. ``--jobs`` is a hidden 0.13 compatibility alias.
    """
    from dagrun import cgroup as cg

    explicit = getattr(ns, "max_cpus", None)
    requested = (
        int(explicit)
        if isinstance(explicit, int)
        else min(container_core_budget(), cg.aggregate_slice_max_cpus())
    )
    requested = max(1, requested)
    if isinstance(ns.cores, int) and requested > ns.cores:
        print(
            f"{PROG}: --cores {ns.cores} is tighter than --max-cpus {requested}; using total "
            f"CPU limit {ns.cores}",
            file=sys.stderr,
        )
        requested = int(ns.cores)
    if inherited_max_cpus is not None and requested > inherited_max_cpus:
        print(
            f"{PROG}: requested/default --max-cpus {requested} exceeds inherited delegated "
            f"CPU ceiling {inherited_max_cpus}; using total CPU limit {inherited_max_cpus}",
            file=sys.stderr,
        )
        requested = inherited_max_cpus
    return requested


def _select_max_mem_bytes(
    ns: argparse.Namespace, inherited_max_mem_bytes: int | None = None
) -> int | None:
    """Resolve the run memory budget, bounded by a delegated parent's effective ceiling."""
    raw = ns.max_mem if isinstance(ns.max_mem, str) and ns.max_mem else None
    requested = _requested_max_mem_bytes(raw)
    if inherited_max_mem_bytes is None:
        return requested
    if raw is not None and requested is None:
        if parse_size(raw) is None:
            print(
                f"{PROG}: could not parse --max-mem {raw!r}; ignoring that request but "
                f"retaining inherited delegated memory ceiling {inherited_max_mem_bytes} bytes",
                file=sys.stderr,
            )
            return inherited_max_mem_bytes
        # Preserve the existing non-positive refusal in _select_max_steps.
        return None
    if requested is None:
        print(
            f"{PROG}: inherited delegated memory ceiling {inherited_max_mem_bytes} bytes is "
            "the effective default --max-mem budget",
            file=sys.stderr,
        )
        return inherited_max_mem_bytes
    if requested > inherited_max_mem_bytes:
        print(
            f"{PROG}: --max-mem {requested} bytes exceeds inherited delegated memory ceiling "
            f"{inherited_max_mem_bytes} bytes; using {inherited_max_mem_bytes} bytes",
            file=sys.stderr,
        )
        return inherited_max_mem_bytes
    return requested


def _uses_inherited_memory_budget(
    ns: argparse.Namespace,
    inherited_max_mem_bytes: int | None,
) -> bool:
    """Whether the parent's ceiling, rather than a tighter valid CLI request, chose the budget."""
    if inherited_max_mem_bytes is None:
        return False
    raw = ns.max_mem if isinstance(ns.max_mem, str) and ns.max_mem else None
    requested = _requested_max_mem_bytes(raw)
    return requested is None or requested > inherited_max_mem_bytes


def _inherited_cores_error(
    ns: argparse.Namespace, inherited_max_cpus: int | None
) -> str | None:
    """Reject an exact cpuset reservation that the delegated quota cannot deliver."""
    cores = ns.cores if isinstance(ns.cores, int) else None
    if cores is None or inherited_max_cpus is None or cores <= inherited_max_cpus:
        return None
    return (
        f"--cores {cores} requests an exact reservation wider than the inherited delegated "
        f"CPU ceiling {inherited_max_cpus}; refusing"
    )


def _select_max_steps(
    cfg: DagConfig,
    ns: argparse.Namespace,
    max_cpus: int,
    effective_max_mem_bytes: int | None = None,
    inherited_memory_budget: bool = False,
) -> int:
    """Resolve the active-step ceiling, combining explicit and memory-derived limits."""
    explicit = int(ns.max_steps) if isinstance(ns.max_steps, int) else None
    base = explicit if explicit is not None else max_cpus
    max_mem = ns.max_mem if isinstance(ns.max_mem, str) and ns.max_mem else None
    if max_mem is None and effective_max_mem_bytes is None:
        return base

    budget = (
        effective_max_mem_bytes
        if effective_max_mem_bytes is not None
        else parse_size(max_mem or "")
    )
    if budget is None:
        print(
            f"{PROG}: could not parse --max-mem {max_mem!r}; falling back to --max-steps "
            f"{base}",
            file=sys.stderr,
        )
        return base

    budget_source = (
        f"--max-mem {max_mem}"
        if max_mem is not None and parse_size(max_mem) == budget
        else f"effective delegated --max-mem {budget} bytes"
    )
    sized_cfg = (
        dataclasses.replace(cfg, mem_cap_floor_bytes=min(cfg.mem_cap_floor_bytes, budget))
        if inherited_memory_budget
        else cfg
    )
    memory_steps, footprint = jobs_for_budget(sized_cfg, budget)
    if memory_steps == 0:
        print(
            f"{PROG}: {budget_source}: REFUSED — minimum runnable footprint "
            f"{footprint} bytes cannot fit safely within budget {budget} bytes",
            file=sys.stderr,
        )
        return 0
    selected = min(base, memory_steps)
    print(
        f"{PROG}: {budget_source} -> modeled memory ceiling {memory_steps} active steps "
        f"(worst-case {footprint} bytes fits budget {budget} bytes); base active-step ceiling "
        f"{base}; final --max-steps {selected}",
        file=sys.stderr,
    )
    ncpu = os.cpu_count() or 4
    modeled = any(
        step.skip_reason is None
        and (
            (step.hint.hard_mem_max_bytes or 0) > 0
            or (step.hint.rss_baseline_bytes or 0) > 0
            or (cfg.default_step_mem_cap_bytes or 0) > 0
        )
        for step in cfg.steps
    )
    if memory_steps == ncpu and not modeled:
        print(
            f"{PROG}: note: no runnable step has a positive hard/RSS/default memory cap, so "
            f"the modeled footprint is only the effective mem_cap_floor_bytes floor "
            f"({sized_cfg.mem_cap_floor_bytes} bytes) and {budget_source} did not throttle "
            f"(modeled memory ceiling {memory_steps} = CPU count; final --max-steps "
            f"{selected})",
            file=sys.stderr,
        )
    return selected


def _effective_run_timeout(ns: argparse.Namespace) -> int | None:
    """The outer run budget from ``--run-timeout``, else the env fallback.

    The env fallback exists so a wrapper that cannot edit the command line (a CI job template, a
    systemd unit) can still bound the run.
    """
    explicit = getattr(ns, "run_timeout", None)
    if explicit:
        return int(explicit)
    raw = os.environ.get("DAGRUN_RUN_TIMEOUT", "")
    if not raw:
        return None
    try:
        value = int(raw)
    except ValueError:
        print(
            f"{PROG}: DAGRUN_RUN_TIMEOUT={raw!r} is not a positive integer; ignoring",
            file=sys.stderr,
        )
        return None
    if value <= 0:
        print(
            f"{PROG}: DAGRUN_RUN_TIMEOUT={raw!r} is not a positive integer; ignoring",
            file=sys.stderr,
        )
        return None
    return value


def _scope_grace_s(run_timeout_s: int) -> int:
    """How much longer than the runner's own budget the SCOPE is allowed to live.

    The scope bound is a backstop for the runner itself wedging, so it must never be the thing
    that fires in normal operation — the runner needs this window to terminate its steps, flush
    profile rows, and return a verdict. Sized as the larger of 60s and a tenth of the budget,
    because reaping a large fan-out is not a constant-time operation.
    """
    return max(60, run_timeout_s // 10)


def _requested_max_mem_bytes(max_mem: str | None) -> int | None:
    """The ``--max-mem`` spec as an outer-scope ceiling in bytes, or ``None`` when it is absent,
    unparseable, or NON-POSITIVE.

    A bad spec is NOT an error here, and non-positive is treated exactly like unparseable:
    :func:`_select_max_steps` already reports both by name and the run exits 2 before any step
    starts (``--max-mem 0``: "REFUSED — minimum runnable footprint … cannot fit safely within
    budget 0 bytes").  Refusing again at scope bring-up would turn one typo into two different
    exit codes depending on whether boxing was attempted — a run with ``--allow-cgroup-failure``
    would exit 2 and the same command without it would exit 3.

    SO THE SPEC IS DROPPED HERE AND REFUSED THERE; it is not accepted and it is not ignored.
    :func:`cgroup.outer_memory_max_bytes`'s own refusal of a non-positive request is a contract
    for library callers, which this function is not one of; the end-to-end behaviour is pinned in
    ``tests/test_max_mem_enforcement.py``.
    """
    if not isinstance(max_mem, str) or not max_mem:
        return None
    parsed = parse_size(max_mem)
    return parsed if parsed is not None and parsed > 0 else None


def _resolve_cgroup_manager(
    allow_failure: bool,
    unsafe_no_cgroups: bool = False,
    max_cpus: int | None = None,
    run_timeout_s: int | None = None,
    max_mem_bytes: int | None = None,
) -> tuple[CgroupManager | None, int]:
    """Establish the two-level cgroup-v2 boxing that is this tool's PRIMARY purpose.

    Cgroup boxing is ON BY DEFAULT. This returns ``(manager, 0)`` when boxing is active (the
    caller runs boxed), ``(None, 0)`` for an intentional best-effort UNBOXED run, or
    ``(None, <nonzero>)`` when boxing is REQUIRED but unavailable and the caller must exit with
    that code (No Silent Failure: the reason is printed to stderr first).

    ``unsafe_no_cgroups`` (``--unsafe-no-cgroups``) is a DELIBERATE opt-out: skip scope bring-up
    entirely and run unboxed even where boxing is available. It is distinct from ``allow_failure``
    (``--allow-cgroup-failure``), which TRIES to box and only downgrades when boxing is
    unavailable. The deliberate opt-out is logged loudly (the reason is a reviewable audit signal),
    and takes precedence over ``allow_failure`` when both are set.

    Flow (mirrors the originating validator's cgroup bring-up):

    * Already inside the managed scope (``DAGRUN_IN_SCOPE=1``): construct :class:`Cgroups`. If
      per-step containment came up, install the scope teardown handler and box. If it did not,
      error (or, with ``allow_failure``, warn and run unboxed).
    * Not in a scope, ``allow_failure``: skip the re-exec and run unboxed with a visible warning
      (today's un-boxed behavior).
    * Not in a scope, default: re-exec this process inside a transient ``systemd-run --user
      --scope`` (a delegated cgroup) via :func:`reexec_in_scope`; on success ``execvp`` replaces
      this process and never returns. If it cannot (no cgroup-v2 / no systemd --user scope, or
      skipped in CI), print a clear error and return a nonzero exit code.
    """
    from dagrun import cgroup as cg

    if unsafe_no_cgroups:
        print(
            f"{PROG}: WARNING: DELIBERATELY UNBOXED via --unsafe-no-cgroups: per-step "
            "memory/CPU-bandwidth/pids caps are NOT enforced; the per-step CPU-time budget uses only a best-effort procfs process-group floor. This is an explicit, reviewable opt-out of "
            "cgroup resource boxing (not a capability fallback).",
            file=sys.stderr,
        )
        return None, 0

    naming = cg.DEFAULT_NAMING
    delegated_root, delegated_error = cg.delegated_cgroup_root(naming)
    if delegated_error is not None:
        print(
            f"{PROG}: ERROR: invalid ${cg.DELEGATED_CGROUP_ENV}: {delegated_error}.",
            file=sys.stderr,
        )
        return None, 3
    if delegated_root is not None:
        manager = cg.Cgroups.from_delegated_root(
            delegated_root,
            naming,
            max_cpus=max_cpus,
            max_mem_bytes=max_mem_bytes,
        )
        if manager.enabled:
            print(
                f"{PROG}: cgroup boxing ACTIVE in parent-owned delegated step root "
                f"{delegated_root}; the outer scheduler retains subtree teardown ownership.",
                file=sys.stderr,
            )
            return manager, 0
        if not manager.close():
            print(
                f"{PROG}: ERROR: failed nested cgroup setup also failed to restore the "
                "scheduler to its prior delegated cgroup; no step will be started.",
                file=sys.stderr,
            )
            return None, 3
        if allow_failure:
            print(
                f"{PROG}: warning: delegated root {delegated_root} could not be subdivided; "
                "running inside the parent-owned boundary without inner per-step cgroups "
                "(--allow-cgroup-failure).",
                file=sys.stderr,
            )
            return None, 0
        print(
            f"{PROG}: ERROR: delegated root {delegated_root} could not establish inner "
            "per-step cgroups.",
            file=sys.stderr,
        )
        return None, 3
    delegated_unboxed = os.environ.get(cg.DELEGATED_UNBOXED_ENV)
    if delegated_unboxed is not None:
        if delegated_unboxed != "1" or not os.environ.get("DAGRUN_OUTER_RUN"):
            print(
                f"{PROG}: ERROR: invalid ${cg.DELEGATED_UNBOXED_ENV}: expected exact value "
                "'1' from an outer delegated_children step.",
                file=sys.stderr,
            )
            return None, 3
        print(
            f"{PROG}: WARNING: reviewed uncontained nested execution via "
            f"${cg.DELEGATED_UNBOXED_ENV}=1; the outer scheduler has no cgroup subtree to "
            "delegate, so this run uses process-group/procfs fallback teardown and accounting.",
            file=sys.stderr,
        )
        return None, 0
    if os.environ.get(naming.env_in_scope) == "1":
        # THE SENTINEL IS A PROMISE; GO AND LOOK BEFORE CLAIMING ANYTHING. This branch reads an
        # environment variable this process set for itself, and the "boxing ACTIVE" line below
        # rested on it. A promised unit is REQUIRED: the re-exec sets sentinel and unit together,
        # and without one "observed in some cgroup" is true of almost every process on a cgroup-v2
        # host -- which would wave through exactly the forged claim this check exists to catch.
        unit = cg.promised_unit(naming=naming)
        evidence = (
            cg.observe_own_containment(unit, naming=naming)
            if unit
            else cg.ContainmentEvidence(
                None,
                "the in-scope sentinel is set but no scope unit was carried with it; the re-exec "
                "always sets both, so this claim names no cgroup to check",
            )
        )
        if evidence.proof is None:
            msg = (
                "the in-scope sentinel is set but containment could NOT be observed: "
                f"{evidence.describe()}"
            )
            if allow_failure:
                print(
                    f"{PROG}: warning: {msg}; running UNBOXED (--allow-cgroup-failure).",
                    file=sys.stderr,
                )
                return None, 0
            print(
                f"{PROG}: ERROR: {msg}. Refusing to report boxing on the strength of an "
                "environment variable.",
                file=sys.stderr,
            )
            return None, 3
        expected_memory_max = cg.expected_outer_memory_max_bytes()
        expected_cpu_count = cg.expected_outer_cpu_count()
        cpu_request_matches = max_cpus is None or expected_cpu_count == max_cpus
        controls_ok = (
            expected_memory_max is not None
            and cpu_request_matches
            and cg.enable_outer_oom_group(naming=naming)
            and cg.verify_scope_limits(expected_memory_max, expected_cpu_count, naming=naming)
        )
        if not controls_ok:
            msg = (
                "outer scope MemoryMax/MemorySwapMax/memory.oom.group/cpu.max readback failed; "
                "the run is not safely contained"
            )
            if allow_failure:
                print(
                    f"{PROG}: warning: {msg}; running best-effort UNBOXED "
                    "(--allow-cgroup-failure).",
                    file=sys.stderr,
                )
                return None, 0
            print(f"{PROG}: ERROR: {msg}.", file=sys.stderr)
            return None, 3
        manager = cg.Cgroups(naming)
        if manager.enabled:
            cg.install_scope_teardown(naming=naming)
            # NAME THE OBSERVED CGROUP, not the intention, so a reader can check the claim
            # against /sys/fs/cgroup instead of taking the word ACTIVE for it.
            print(
                f"{PROG}: cgroup boxing ACTIVE (two-level cgroup-v2 scope; per-step memory/CPU caps"
                f" + setsid-proof teardown); containment OBSERVED: {evidence.describe()}.",
                file=sys.stderr,
            )
            return manager, 0
        if allow_failure:
            print(
                f"{PROG}: warning: inside a scope but per-step cgroup setup failed; running "
                "best-effort UNBOXED (--allow-cgroup-failure).",
                file=sys.stderr,
            )
            return None, 0
        print(
            f"{PROG}: ERROR: {CGROUP_SETUP_ENVIRONMENT_ERROR}; re-run with "
            "--allow-cgroup-failure to run UNBOXED.",
            file=sys.stderr,
        )
        return None, 3
    if allow_failure:
        print(
            f"{PROG}: warning: cgroup boxing not established (--allow-cgroup-failure); running "
            "UNBOXED (process-group teardown, no per-step memory/CPU-bandwidth caps, and only a best-effort procfs CPU-time floor).",
            file=sys.stderr,
        )
        # SAY WHICH BOUNDS SURVIVE THE FALLBACK, because "unboxed" has been read as "unbounded"
        # and that reading is how a run reached an external job kill. Per-step WALL budgets and
        # the outer run budget are enforced by the runner itself and still apply; the per-step
        # CPU-time budget and the scope's RuntimeMaxSec are cgroup/systemd features and do not.
        if run_timeout_s:
            print(
                f"{PROG}: unboxed run is STILL wall-bounded: per-step wall timeouts apply and "
                f"the whole run is cut at {run_timeout_s}s. Per-step CPU-time budgets use a best-effort procfs process-group floor; the "
                "scope-level RuntimeMaxSec backstop is NOT enforced without cgroups.",
                file=sys.stderr,
            )
        else:
            print(
                f"{PROG}: WARNING: no outer run budget is set (--run-timeout), so nothing bounds "
                "the run as a whole; only per-step wall timeouts apply.",
                file=sys.stderr,
            )
        return None, 0
    # Default: boxing is required -> re-exec into a transient systemd --user scope.
    # Re-exec through __main__.py by absolute path (NOT '-m dagrun'): the tool is invoked
    # via the py/bin symlink without a pip install, so a fresh child interpreter's
    # sys.path lacks py/ and '-m dagrun' dies with 'No module named dagrun'. __main__.py
    # does its own sys.path fixup, so invoking it directly imports the package cleanly.
    # This keeps default-on boxing working outside CI (local validate.sh), not just in
    # Actions where boxing is skipped.
    _main_py = os.path.join(os.path.dirname(os.path.abspath(__file__)), "__main__.py")
    argv = [sys.executable, _main_py, *sys.argv[1:]]
    outer_memory_max = cg.outer_memory_max_bytes(max_mem_bytes)
    if outer_memory_max is None:
        print(
            f"{PROG}: ERROR: cannot derive a positive outer MemoryMax from MemAvailable/"
            f"${cg.OUTER_MEMORY_MAX_ENV}/--max-mem; refusing an unbounded run.",
            file=sys.stderr,
        )
        return None, 3
    # SAY WHICH CEILING WON. --max-mem used to size the schedule and nothing else, so a run could
    # report a 20 GiB budget while its scope admitted 90% of the host. Naming the binding ceiling
    # is what makes the containment claim checkable against the live unit.
    if max_mem_bytes is not None:
        if outer_memory_max == max_mem_bytes:
            print(
                f"{PROG}: --max-mem is the outer scope ceiling: "
                f"MemoryMax={outer_memory_max} bytes.",
                file=sys.stderr,
            )
        else:
            print(
                f"{PROG}: --max-mem {max_mem_bytes} bytes did not bind; the derived/environment "
                f"boundary is smaller: MemoryMax={outer_memory_max} bytes.",
                file=sys.stderr,
            )
    reexeced_or_skipped = cg.reexec_in_scope(
        argv,
        memory_max=outer_memory_max,
        cpu_count=max_cpus,
        runtime_max_s=(
            run_timeout_s + _scope_grace_s(run_timeout_s) if run_timeout_s else None
        ),
    )
    # Only reached when NO exec happened (execvp on success never returns).
    #
    # NAME WHAT ACTUALLY HAPPENED. This used to pick between two sentences from a bool, and on the
    # policy-skip path it chose "boxing was skipped (e.g. CI without a systemd --user scope)" -- a
    # claim about a capability nothing had tested. The exit code and the policy are unchanged.
    skip_reason = cg.policy_skip_reason()
    if reexeced_or_skipped and skip_reason is not None:
        print(
            f"{PROG}: ERROR: cgroup boxing was NOT ESTABLISHED and NOT TESTED: scope setup was "
            f"skipped by policy because ${skip_reason} is set, so this run does not know whether "
            "boxing is available here. Re-run with --allow-cgroup-failure to run UNBOXED, or with "
            f"{cg.FORCE_ATTEMPT_ENV}=1 to probe instead of skipping.",
            file=sys.stderr,
        )
        return None, 3
    detail = (
        "the scope re-exec returned without entering a scope"
        if reexeced_or_skipped
        else "scope setup was attempted and failed (cgroup-v2 + a working systemd --user scope "
        "are unavailable)"
    )
    print(
        f"{PROG}: ERROR: cgroup boxing could not be established: {detail}. Cgroup resource boxing "
        "is this tool's primary purpose; re-run with --allow-cgroup-failure to run UNBOXED.",
        file=sys.stderr,
    )
    return None, 3


def _cmd_summary(ns: argparse.Namespace) -> int:
    """The ``summary`` subcommand family: build / merge / plan-from / stats the mergeable profile
    summary. These primitives make the summary format inspectable and drivable from a script, and
    are what the cross-language differential exercises for byte-identical serialization + merge."""
    action = ns.summary_command if isinstance(ns.summary_command, str) else None
    if action == "build":
        return _cmd_summary_build(ns)
    if action == "merge":
        return _cmd_summary_merge(ns)
    if action == "plan":
        return _cmd_summary_plan(ns)
    if action == "stats":
        return _cmd_summary_stats(ns)
    print(
        f"{PROG}: summary: an action is required (build | merge | plan | stats)", file=sys.stderr
    )
    return 2


def _emit_summary(text: str, out: str | None) -> int:
    """Write summary JSON to ``out`` (with a trailing newline) or stdout."""
    if out:
        try:
            Path(out).write_text(text + "\n", encoding="utf-8")
        except OSError as exc:
            print(f"{PROG}: summary: cannot write {out}: {exc}", file=sys.stderr)
            return 2
    else:
        print(text)
    return 0


def _cmd_summary_build(ns: argparse.Namespace) -> int:
    """Build a summary JSON from the profile-store CSV for the CURRENT feedback identity."""
    perf_dir = _resolve_feedback_dir(ns.perf_dir, False)
    machine_id, container_class = feedback_identity()
    from dagrun.estimates import _affinity_width, _load_store

    if perf_dir is None:
        summary = summarylib.empty(machine_id, container_class)
    else:
        loaded = _load_store(perf_dir, machine_id, container_class)
        if loaded is None:
            summary = summarylib.empty(machine_id, container_class)
        else:
            rows, affinity = loaded
            summary = summarylib.summary_from_rows(
                rows, machine_id, container_class, affinity, reservoir_cap=int(ns.reservoir_cap)
            )
    return _emit_summary(summarylib.to_json(summary), ns.out if isinstance(ns.out, str) else None)


def _cmd_summary_merge(ns: argparse.Namespace) -> int:
    """Merge one or more summary JSON files (order-independent) into one on stdout / --out."""
    files = list(ns.files)
    try:
        summaries = [
            summarylib.from_json(Path(f).read_text(encoding="utf-8")) for f in files
        ]
    except (OSError, summarylib.SummaryError) as exc:
        print(f"{PROG}: summary merge: {exc}", file=sys.stderr)
        return 2
    if not summaries:
        print(f"{PROG}: summary merge: need at least one file", file=sys.stderr)
        return 2
    first = summaries[0]
    try:
        merged = summarylib.merge_all(
            summaries,
            first.machine_id,
            first.container_class,
            reservoir_cap=int(ns.reservoir_cap),
        )
    except summarylib.SummaryError as exc:
        print(f"{PROG}: summary merge: {exc}", file=sys.stderr)
        return 2
    return _emit_summary(summarylib.to_json(merged), ns.out if isinstance(ns.out, str) else None)


def _cmd_summary_plan(ns: argparse.Namespace) -> int:
    """Build a plan fed by a summary JSON (same output shape as `plan`)."""
    try:
        summary = summarylib.from_json(Path(str(ns.summary)).read_text(encoding="utf-8"))
    except (OSError, summarylib.SummaryError) as exc:
        print(f"{PROG}: summary plan: {exc}", file=sys.stderr)
        return 2
    try:
        cfg = _load(str(ns.dag))
    except (OSError, DagJsonError) as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2
    planner = Planner.from_value(str(ns.planner)) or Planner.GREEDY_LPT
    max_mem = ns.max_mem if isinstance(ns.max_mem, str) and ns.max_mem else None
    core_budget, mem_budget = _planning_budgets(planner, max_mem)
    plan = _build_plan_from_summary(
        cfg,
        summary,
        planner,
        core_budget=core_budget,
        mem_budget=mem_budget,
        max_steps=(int(ns.max_steps) if isinstance(ns.max_steps, int) else None),
    )
    if str(ns.format) == "json":
        print(plan_to_json(plan))
    else:
        sys.stdout.write(plan_to_text(plan))
    return 0


def _cmd_summary_stats(ns: argparse.Namespace) -> int:
    """Print the bounded-size witness: bucket_count / total_samples / max_bucket_samples."""
    try:
        summary = summarylib.from_json(Path(str(ns.file)).read_text(encoding="utf-8"))
    except (OSError, summarylib.SummaryError) as exc:
        print(f"{PROG}: summary stats: {exc}", file=sys.stderr)
        return 2
    buckets, total, largest = summarylib.summary_stats(summary)
    print(
        f"identity: {summary.machine_id}/{summary.container_class}\n"
        f"buckets: {buckets}\ntotal_samples: {total}\nmax_bucket_samples: {largest}"
    )
    return 0


def _cmd_plan(ns: argparse.Namespace) -> int:
    """Show the plan (per-step estimates + sources, critical path, scheduled order) without
    running anything. Text (human) or canonical JSON (machine-readable, cross-identical)."""
    dag_arg = ns.dag if isinstance(ns.dag, str) else None
    if dag_arg is None:
        print(f"{PROG}: plan: --dag FILE is required", file=sys.stderr)
        return 2
    try:
        cfg = _load(dag_arg)
    except (OSError, DagJsonError) as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2
    planner = Planner.from_value(str(ns.planner)) or Planner.GREEDY_LPT
    feedback_dir = _resolve_feedback_dir(ns.perf_dir, bool(ns.no_profile_feedback))
    max_mem = ns.max_mem if isinstance(ns.max_mem, str) and ns.max_mem else None
    core_budget, mem_budget = _planning_budgets(planner, max_mem)
    plan = _build_feedback_plan(
        cfg,
        feedback_dir,
        planner,
        core_budget=core_budget,
        mem_budget=mem_budget,
        max_steps=(int(ns.max_steps) if isinstance(ns.max_steps, int) else None),
    )
    if str(ns.format) == "json":
        print(plan_to_json(plan))
    else:
        sys.stdout.write(plan_to_text(plan))
    return 0


#: Default per-command name when ``box --label`` is absent and the basename yields nothing usable.
_BOX_DEFAULT_LABEL = "command"

#: The modeled-footprint floor an ordinary DAG gets, READ FROM THE MODEL rather than copied here.
#: `box` may only ever LOWER it (to a `--mem` the caller stated), and a copied constant is exactly
#: what would let the two drift apart without anything noticing.
_DEFAULT_MEM_CAP_FLOOR_BYTES: int = DagConfig(steps=()).mem_cap_floor_bytes


def _box_step_and_config(
    argv: Sequence[str], *, label: str, mem_bytes: int | None, timeout_s: int, cores: int
) -> DagConfig:
    """Build the singleton DAG that ``box`` runs, so ONE command needs no DAG file.

    Deliberately a real :class:`DagConfig` handed to the ordinary run path rather than a separate
    execution route. `box` must be indistinguishable from hand-writing the equivalent
    singleton-DAG file -- that is its entire value -- and the only way to guarantee that is for it
    to be the same code after this function returns.

    ARGV IS SHELL-QUOTED, NOT JOINED. A step's ``cmd`` is handed to ``bash -c``, so an unquoted
    join would let an argument containing a space, a quote, a ``;`` or a ``$(...)`` become shell
    syntax rather than an argument. :func:`shlex.quote` per element makes every element survive as
    exactly one word.

    ``--mem`` ALSO LOWERS THE MODELED FLOOR, or the flag is unusable for the values anyone
    actually types. ``DagConfig.mem_cap_floor_bytes`` defaults to 8 GiB: a lower bound on the
    modeled worst-case footprint so that sizing an UNCHARACTERIZED graph never concludes "zero
    steps fit". A boxed command is the opposite of uncharacterized -- ``--mem`` states its hard
    inner cap exactly -- so leaving the floor at 8 GiB made the run's own budget check
    (``--max-mem`` -> :func:`jobs_for_budget`) compare a 512 MiB budget against a fictional 8 GiB
    step and REFUSE, for every value below the very default the flag was reached for in order to
    lower. Pinned by ``test_a_mem_below_the_modeled_floor_still_runs_the_command``.
    """
    return DagConfig(
        steps=(
            Step(
                "box",
                label,
                " ".join(argv),
                " ".join(shlex.quote(part) for part in argv),
                timeout=timeout_s,
                # THE CPU CEILING IS DERIVED, NOT DEFAULTED. Left unset, the step would inherit
                # the deliberately tiny 10-second per-step CPU floor, which exists as a forcing
                # function for an UNDECLARED DAG node -- and would cut an honest boxed command
                # short for a reason its author never asked about. `--timeout x --cores` is the
                # most CPU the command could possibly consume inside its wall budget, so the wall
                # bound is the one that fires and the CPU guard stays a backstop.
                cpu_timeout=timeout_s * cores,
                hint=ResourceHint(hard_mem_max_bytes=mem_bytes),
            ),
        ),
        # cpu.max for the boxed command. Set on the CONFIG rather than as preferred_inner_jobs,
        # because the latter appends a `-j K` flag to the command -- correct for a build, wrong
        # for an arbitrary command that may not accept one.
        default_step_cpu_count=cores,
        mem_cap_floor_bytes=(
            _DEFAULT_MEM_CAP_FLOOR_BYTES
            if mem_bytes is None
            else min(_DEFAULT_MEM_CAP_FLOOR_BYTES, mem_bytes)
        ),
    )


def _cmd_box(ns: argparse.Namespace, parser: argparse.ArgumentParser, c: Palette) -> int:
    """Box ONE command: synthesize the singleton DAG, then take the ordinary run path."""
    argv = [part for part in (ns.command_argv or []) if part != "--"]
    if not argv:
        print(f"{PROG}: box: no command given (use '-- CMD [ARGS...]')", file=sys.stderr)
        return 2

    label = ns.label if isinstance(ns.label, str) and ns.label.strip() else None
    if label is None:
        label = os.path.basename(argv[0]).strip() or _BOX_DEFAULT_LABEL
    # The tag is `group.job`, so a dot in the job half would produce a tag that reads as a
    # different group. Replace rather than refuse: a label is a convenience, not a declaration.
    label = label.replace(".", "-")

    cores = int(ns.cores) if ns.cores is not None else 1
    timeout_s = int(ns.timeout) if ns.timeout is not None else DEFAULT_STEP_TIMEOUT
    mem_bytes = _requested_max_mem_bytes(ns.mem if isinstance(ns.mem, str) else None)
    if isinstance(ns.mem, str) and ns.mem and mem_bytes is None:
        print(
            f"{PROG}: box: --mem {ns.mem!r} is not a positive size (e.g. 512M, 8G)",
            file=sys.stderr,
        )
        return 2

    cfg = _box_step_and_config(
        argv, label=label, mem_bytes=mem_bytes, timeout_s=timeout_s, cores=cores
    )

    # EVERY OTHER KNOB COMES FROM `run`'s OWN DEFAULTS, obtained by parsing a bare `run`
    # invocation, rather than from a second list of defaults maintained here. A hand-copied list
    # is exactly the thing that drifts the moment `run` gains a flag, and the drift would be
    # invisible: `box` would keep working while quietly diverging from the singleton DAG file it
    # claims to be shorthand for.
    forwarded = ["run", "--dag", "-", "-s", "1", "-j", str(cores)]
    if ns.perf_dir is not None:
        forwarded += ["--perf-dir", str(ns.perf_dir)]
    if bool(ns.allow_cgroup_failure):
        forwarded.append("--allow-cgroup-failure")
    if bool(ns.quiet):
        forwarded.append("-q")
    if ns.mem is not None:
        forwarded += ["--max-mem", str(ns.mem)]
    run_ns = parser.parse_args(forwarded)
    return _run(cfg, run_ns, c)


#: Exit code for a run that admission would not let start. Distinct from 2 (bad usage) and 3
#: (cgroup boxing unavailable) on purpose: a scheduler that retries should be able to tell "this
#: host is busy, come back" from "this invocation is wrong", without parsing prose.
ADMISSION_EXIT_CODE = 4

#: Largest ``--admission WAIT_S`` this CLI will accept, in seconds (one day).
#:
#: An UPPER bound, not only a lower one, because "finite and >= 0" admits 1e19 -- a number no
#: operator means and no CI job can outlive. The paired Rust engine builds its deadline with
#: `Instant + Duration`, which PANICS (exit 101, "overflow when adding duration to instant") on
#: exactly such a value, so an unbounded WAIT_S was an input one engine validated and then
#: aborted on. A day is longer than any real queue and short enough to be arithmetic.
MAX_ADMISSION_WAIT_S = 86400.0


def _apply_admission(
    ns: argparse.Namespace, effective_max_mem_bytes: int | None = None
) -> int:
    """Reserve this run's memory against the host-wide ledger, or refuse to start. 0 = proceed.

    Absent ``--admission`` this is a no-op: admission is opt-in because a durable cross-process
    ledger changes when a run may start, and that is not something to switch on underneath
    existing callers.
    """
    raw = getattr(ns, "admission", None)
    if raw is None:
        return 0
    try:
        wait_s = float(raw)
    except (TypeError, ValueError):
        print(
            f"{PROG}: run: --admission WAIT_S must be a number of seconds (got {raw!r})",
            file=sys.stderr,
        )
        return 2
    # NaN must be refused too, hence the explicit finite check: a wait budget that is not a
    # number is a usage error, and `nan >= 0` is False while `nan < 0` is also False. The UPPER
    # bound is refused in the same breath and with the same words as the lower one: see
    # MAX_ADMISSION_WAIT_S for why an unbounded wait is not merely silly but unsafe.
    if not math.isfinite(wait_s) or wait_s < 0 or wait_s > MAX_ADMISSION_WAIT_S:
        print(
            f"{PROG}: run: --admission WAIT_S must be a finite number of seconds in "
            f"[0, {MAX_ADMISSION_WAIT_S:g}] (got {raw!r})",
            file=sys.stderr,
        )
        return 2

    requested = (
        effective_max_mem_bytes
        if effective_max_mem_bytes is not None
        else _requested_max_mem_bytes(getattr(ns, "max_mem", None))
    )
    if requested is None:
        # REQUIRED, not guessed. The only numbers available without --max-mem describe the whole
        # host, so guessing would reserve everything and turn admission into a global mutex --
        # which would look like it was working right up until it deadlocked a CI fleet.
        print(
            f"{PROG}: run: --admission requires --max-mem or a finite inherited delegated "
            "memory ceiling: admission reserves a NUMBER against a host-wide ledger.",
            file=sys.stderr,
        )
        return 2

    # ALREADY ADMITTED -- AND THIS RUN PROVES IT IS THE ONE ADMITTED. Boxing re-execs this process
    # into a systemd scope with `execvp`, which keeps the pid AND the /proc start time, so the
    # record this run wrote before the exec is still its own live reservation. Asking again would
    # count this run twice against the budget -- and on a tight budget the second ask would QUEUE
    # behind the first, i.e. the run would wait for itself until the wait ran out.
    #
    # THE SENTINEL ALONE CANNOT ESTABLISH THAT. `systemd-run --setenv=DAGRUN_IN_SCOPE=1` sets it
    # for the whole scope, and every step's environment is built from `os.environ`, so a runner
    # invoked as a STEP of a boxed run inherits the same "1" while holding no reservation at all
    # -- and skipping on the flag alone would wave that nested run through with nothing reserved
    # and no verdict printed. So ask the LEDGER whether a live record is fingerprinted with this
    # pid and this /proc start time: only its own record licenses the skip. Anything else falls
    # through and admits normally.
    from dagrun import cgroup as cg

    if os.environ.get(cg.DEFAULT_NAMING.env_in_scope) == "1":
        try:
            own_bytes = admission.held_by_this_process()
        except admission.AdmissionStateError as error:
            print(f"{PROG}: run: admission ledger unusable: {error}", file=sys.stderr)
            return ADMISSION_EXIT_CODE
        if own_bytes > 0:
            return 0

    try:
        decision, reservation = admission.admit(
            requested, tag="run", wait_s=wait_s, poll_s=min(2.0, max(0.25, wait_s / 4))
        )
    except admission.AdmissionStateError as error:
        # A ledger this process cannot read is not permission to proceed: it is the shared state
        # admission exists to consult, and guessing past it would silently restore the contention.
        print(f"{PROG}: run: admission ledger unusable: {error}", file=sys.stderr)
        return ADMISSION_EXIT_CODE
    if reservation is not None:
        return 0
    print(f"{PROG}: run: {decision.reason}", file=sys.stderr)
    if decision.verdict is admission.Verdict.QUEUE and wait_s <= 0:
        print(
            f"{PROG}: run: pass --admission SECONDS to wait for a slot instead of exiting.",
            file=sys.stderr,
        )
    return ADMISSION_EXIT_CODE


def _run(cfg: DagConfig, ns: argparse.Namespace, c: Palette) -> int:
    timeseries_value = getattr(ns, "profile_timeseries", None)
    timeseries_raw = timeseries_value if isinstance(timeseries_value, str) else None
    if timeseries_raw is not None and bool(ns.no_profile):
        print(
            f"{PROG}: run: --profile-timeseries cannot be combined with --no-profile",
            file=sys.stderr,
        )
        return 2
    profile_timeseries_interval_s: float | None = None
    if timeseries_raw is not None:
        try:
            profile_timeseries_interval_s = _parse_profile_timeseries_duration(timeseries_raw)
        except ValueError as exc:
            print(f"{PROG}: run: {exc}", file=sys.stderr)
            return 2

    # A selection includes its full dependency ancestry by default. Validate and select BEFORE
    # cgroup bring-up so a bad combination or tag fails fast (exit 2) without needing a scope.
    source_step_count = len(cfg.steps)
    selected_raw = ns.selected if isinstance(ns.selected, str) else None
    labels_raw = ns.labels if isinstance(ns.labels, str) else None
    ignore_selected_deps = bool(ns.ignore_selected_deps)
    if selected_raw is not None and labels_raw is not None:
        print(f"{PROG}: run: --selected and --labels cannot be combined", file=sys.stderr)
        return 2
    if ignore_selected_deps and selected_raw is None:
        print(
            f"{PROG}: run: --ignore-selected-deps requires --selected",
            file=sys.stderr,
        )
        return 2
    if selected_raw is not None:
        tags = _parse_tag_list(selected_raw)
        if not tags:
            print(f"{PROG}: run: --selected requires at least one tag", file=sys.stderr)
            return 2
        try:
            cfg = _select_steps(cfg, tags, ignore_selected_deps=ignore_selected_deps)
        except _SelectedError as exc:
            print(f"{PROG}: {exc}", file=sys.stderr)
            return 2
    elif labels_raw is not None:
        labels = _parse_tag_list(labels_raw)
        if not labels:
            print(f"{PROG}: run: --labels requires at least one label", file=sys.stderr)
            return 2
        try:
            cfg = _select_steps_by_labels(cfg, labels)
        except _SelectedError as exc:
            print(f"{PROG}: {exc}", file=sys.stderr)
            return 2
    selected_subgraph = len(cfg.steps) < source_step_count

    # Feature: --args — forward extra args into the selected step(s) that declare the {args}
    # placeholder (scope a step down to specific test case(s)). Applied BEFORE --stress so the
    # scoped command is what gets duplicated.
    try:
        cfg = _apply_passthrough_args(cfg, ns.args if isinstance(ns.args, str) else None)
    except _ArgsError as exc:
        print(f"{PROG}: run: {exc}", file=sys.stderr)
        return 2

    inherited_max_cpus, inherited_max_mem_bytes, inherited_error = (
        _inherited_delegated_limits()
    )
    if inherited_error is not None:
        print(f"{PROG}: ERROR: {inherited_error}.", file=sys.stderr)
        return 3
    if cores_error := _inherited_cores_error(ns, inherited_max_cpus):
        print(f"{PROG}: run: {cores_error}", file=sys.stderr)
        return 2

    # Resolve the maximum CPU capacity before stress sizing or cgroup bring-up. The stress guard
    # must charge the widths that can actually execute under this run's per-step ceiling, not an
    # authored width which will be clamped later. Self-managed commands cannot be clamped, so
    # reject those before either sizing or expansion.
    max_cpus = _select_max_cpus(ns, inherited_max_cpus)
    effective_max_mem_bytes = _select_max_mem_bytes(ns, inherited_max_mem_bytes)
    inherited_memory_budget = _uses_inherited_memory_budget(
        ns, inherited_max_mem_bytes
    )
    if effective_max_mem_bytes is not None and (
        selected_subgraph or inherited_memory_budget
    ):
        # A selected config retains the source DAG's non-step policy, including its conservative
        # whole-graph floor. That floor is not evidence that an omitted node consumes memory.
        # Bound it to this invocation's physical budget before every planner/stress/admission
        # preflight; the selected steps' own hard/default/RSS-derived caps remain unchanged and
        # still refuse a genuinely infeasible selection.
        cfg = dataclasses.replace(
            cfg,
            mem_cap_floor_bytes=min(cfg.mem_cap_floor_bytes, effective_max_mem_bytes),
        )
    if error := _self_managed_width_error(cfg, max_cpus):
        print(f"{PROG}: run: {error}", file=sys.stderr)
        return 2

    # Feature: --stress N — duplicate the selected step(s) N times and run the copies in PARALLEL,
    # reporting the per-copy ratio. Derive the memory-safe N ceiling from a CPU-capped,
    # pre-expansion copy and REFUSE LOUDLY when the requested N would exceed the box memory budget,
    # BEFORE any cgroup bring-up / re-exec (so a too-large N fails fast without needing a systemd
    # scope).
    stress_n = int(ns.stress) if getattr(ns, "stress", None) is not None else 1
    if stress_n < 1:
        print(f"{PROG}: run: --stress N must be >= 1 (got {stress_n})", file=sys.stderr)
        return 2
    stress_active = stress_n > 1
    if stress_active:
        code = _stress_expansion_guard(cfg, stress_n)
        if code != 0:
            return code
        # Clamp once BEFORE expansion: the guard sizes exactly what will be cloned, and the later
        # post-plan clamp is idempotent instead of warning once for every generated copy.
        cfg = cap_config_max_cpus(cfg, max_cpus)
        code = (
            _stress_guard(cfg, stress_n)
            if effective_max_mem_bytes is None
            else _stress_guard(cfg, stress_n, effective_max_mem_bytes)
        )
        if code != 0:
            return code
        cfg = _expand_stress(cfg, stress_n)

    # HOST-WIDE MEMORY ADMISSION, opt-in, and deliberately BEFORE cgroup bring-up. A run that is
    # going to wait must not be holding a systemd scope while it waits, and a run that is going to
    # be refused should not have created one at all. The reservation is held for the rest of the
    # process (released by admission's atexit hook), so the ledger reflects this run for exactly as
    # long as it is on the machine.
    admission_code = _apply_admission(ns, effective_max_mem_bytes)
    if admission_code != 0:
        return admission_code

    # Bind the resolved CPU capacity to the outer scope's CPUQuota. Declared widths are not
    # reservations: --max-steps, dependencies, and named resources decide which steps overlap.
    cgroups, code = _resolve_cgroup_manager(
        bool(ns.allow_cgroup_failure),
        bool(getattr(ns, "unsafe_no_cgroups", False)),
        max_cpus=max_cpus,
        run_timeout_s=_effective_run_timeout(ns),
        max_mem_bytes=effective_max_mem_bytes,
    )
    if code != 0:
        return code
    if profile_timeseries_interval_s is not None and cgroups is None:
        print(
            f"{PROG}: run: --profile-timeseries requires active cgroup-v2 containment; "
            "no step was started",
            file=sys.stderr,
        )
        return 3

    # Opt-in --cores K: constrain the whole run tree to K reserved cores. Apply it here,
    # after the boxing re-exec has settled (the re-exec'd in-scope child re-enters _run and applies
    # it there) and BEFORE the scheduler spawns any worker thread or forks any step — pthreads
    # inherit the creator's affinity and forked steps inherit at fork, so an early application
    # covers the whole descendant tree. Only an exact cgroup cpuset is accepted; process affinity
    # is escapable and therefore cannot enforce a collision-free reservation.
    core_reservation: Reservation | None = None
    if ns.cores is not None:
        from dagrun import cgroup as _cg
        from dagrun import reservation as _res

        # Reserve disjoint cores first, then require the exact set to become the scope's effective
        # cpuset. Exhaustion or unavailable enforcement fails closed instead of colliding or
        # silently running unpinned.
        try:
            reservation = _res.acquire(int(ns.cores), tag="run")
        except (ValueError, _res.InsufficientCoresError) as exc:
            print(f"{PROG}: --cores {ns.cores}: reservation failed: {exc}", file=sys.stderr)
            return 3
        if _cg.apply_specific_cores(reservation.cores, label=f"--cores {ns.cores}") is None:
            reservation.release()
            print(
                f"{PROG}: --cores {ns.cores}: hard cgroup cpuset unavailable; refusing to run",
                file=sys.stderr,
            )
            return 3
        core_reservation = reservation

    # Make the CPU budget visible to planning as well as execution. Authored widths and the
    # undeclared-step cpu.max default are capped before CPA sees the graph, so --show-plan never
    # advertises a width the scheduler will later change.
    cfg = cap_config_max_cpus(cfg, max_cpus)

    # Plan-time profile-store feedback: refine each step's est_duration_s
    # and rss_baseline_bytes from the recorded store, then pick the dispatch order for the chosen
    # --planner. The applied cfg (with refined hints) is what both the memory-aware --max-steps
    # sizing below and the scheduler see, so planning improves automatically as runs accumulate.
    planner = Planner.from_value(str(ns.planner)) or Planner.GREEDY_LPT
    feedback_dir = _resolve_feedback_dir(ns.perf_dir, bool(ns.no_profile_feedback))
    max_mem = ns.max_mem if isinstance(ns.max_mem, str) and ns.max_mem else None
    core_budget, mem_budget = _planning_budgets(
        planner,
        max_mem,
        max_cpus,
        effective_max_mem_bytes,
    )
    planning_max_steps = (
        int(ns.max_steps) if isinstance(ns.max_steps, int) else max_cpus
    )

    # Profile-artifact SYNC (close the ephemeral-CI feedback loop): parse the backend once; the
    # DOWNLOAD half seeds the planner from the shared summary, the UPLOAD half publishes this run's
    # samples after it finishes. A malformed spec fails fast (exit 2); a backend I/O failure degrades
    # LOUDLY (a warning) without failing the run.
    sync_spec = ns.profile_sync if isinstance(ns.profile_sync, str) and ns.profile_sync else None
    direction = str(ns.profile_sync_direction)
    backend: synclib.SyncBackend | None = None
    if sync_spec is not None:
        try:
            backend = synclib.parse_backend(sync_spec)
        except synclib.SyncError as exc:
            print(f"{PROG}: --profile-sync: {exc}", file=sys.stderr)
            if core_reservation is not None:
                core_reservation.release()
            return 2
    do_download = backend is not None and direction in ("both", "download")
    do_upload = backend is not None and direction in ("both", "upload")

    seed_plan = _sync_seed_plan(
        cfg, backend, feedback_dir, planner, do_download and not bool(ns.no_profile_feedback),
        core_budget=core_budget,
        mem_budget=mem_budget,
        max_steps=planning_max_steps,
    )
    if seed_plan is not None:
        plan = seed_plan
    else:
        plan = _build_feedback_plan(
            cfg,
            feedback_dir,
            planner,
            core_budget=core_budget,
            mem_budget=mem_budget,
            max_steps=planning_max_steps,
        )
    if plan.allocation is not None and plan.allocation.stop_reason == "infeasible-memory":
        print(
            f"{PROG}: run: CPA allocation is infeasible under --max-mem {max_mem}; "
            "the minimum runnable footprint exceeds the memory budget",
            file=sys.stderr,
        )
        if core_reservation is not None:
            core_reservation.release()
        return 2
    authored = cfg
    cfg = cap_config_max_cpus(apply_plan_to_config(cfg, plan), max_cpus)
    # Censoring-aware memory feedback (opt-in), applied AFTER the ordinary plan so it has the last
    # word on rss_baseline_bytes and BEFORE the memory-aware --max-steps sizing that reads it. It
    # is handed the PRE-plan config as well, so a step it declines to estimate goes back to its
    # authored baseline (or a larger proven floor) when this stricter path declines to estimate.
    cfg = _apply_memory_feedback(
        cfg, authored, feedback_dir, bool(getattr(ns, "profile_memory_feedback", False))
    )
    # Compatibility flag: the SMALL forcing-function caps are already active by default. Reassert
    # the same values so older callers keep working and announce that the flag is now redundant.
    if bool(getattr(ns, "small_default_cap", False)):
        cfg = dataclasses.replace(
            cfg,
            default_step_mem_cap_bytes=DEFAULT_SMALL_MEM_CAP_BYTES,
            default_step_cpu_count=DEFAULT_SMALL_CPU_COUNT,
            default_step_cpu_timeout=DEFAULT_SMALL_CPU_TIMEOUT,
        )
        print(
            f"{PROG}: --small-default-cap is redundant (SMALL defaults are already active): "
            f"undeclared steps are boxed to "
            f"(mem {DEFAULT_SMALL_MEM_CAP_BYTES} B / {DEFAULT_SMALL_CPU_COUNT} core / "
            f"{DEFAULT_SMALL_CPU_TIMEOUT} s CPU); declared per-step hints still win",
            file=sys.stderr,
        )
    # Per-platform CPU-budget scaling. Resolved AFTER apply_plan_to_config so the planner never
    # sees (and cannot bake in) a platform-specific number: the graph and the plan stay canonical,
    # and only enforcement is scaled.
    try:
        cpu_multiplier, cpu_platform = resolve_cpu_timeout_multiplier(
            getattr(ns, "cpu_timeout_multiplier", None)
        )
    except ValueError as exc:
        print(f"{PROG}: error: {exc}", file=sys.stderr)
        return 2
    if cpu_multiplier != DEFAULT_CPU_TIMEOUT_MULTIPLIER:
        cfg = dataclasses.replace(
            cfg,
            cpu_timeout_multiplier=cpu_multiplier,
            cpu_timeout_platform=cpu_platform,
        )
        # Announce it: a scaled budget silently in force is exactly the invisible-policy problem
        # this mechanism exists to avoid.
        label = f" ({cpu_platform})" if cpu_platform else ""
        print(
            f"{PROG}: per-platform CPU-budget multiplier x{cpu_multiplier:g}{label} in effect; "
            "every step's canonical cpu_timeout is scaled by it for enforcement on this platform",
            file=sys.stderr,
        )
    if bool(ns.show_plan):
        sys.stdout.write(plan_to_text(plan))

    # The authored-hint preflight ran before cgroup bring-up. Feedback and CPA may have raised RSS
    # estimates or allocated wider CPU-bound steps since then, so re-check the FINAL, already-
    # expanded graph without multiplying by stress_n again. This is the last no-spawn barrier.
    if stress_active:
        code = (
            _final_stress_guard(cfg, stress_n)
            if effective_max_mem_bytes is None
            else _final_stress_guard(cfg, stress_n, effective_max_mem_bytes)
        )
        if code != 0:
            if core_reservation is not None:
                core_reservation.release()
            return code

    max_steps = _select_max_steps(
        cfg,
        ns,
        max_cpus,
        effective_max_mem_bytes,
        inherited_memory_budget=inherited_memory_budget,
    )
    if max_steps < 1:
        if core_reservation is not None:
            core_reservation.release()
        return 2
    verbosity = 0 if bool(ns.quiet) else int(ns.verbosity)

    perf_dir, source = _resolve_profile_dir(ns.perf_dir, bool(ns.no_profile))
    metrics: MetricsSink | None = None
    profile_run_id: str | None = None
    if perf_dir is not None:
        from dagrun.perflog import CsvMetricsSink

        csv_metrics = CsvMetricsSink(
            perf_dir,
            git_sha=_git_sha(),
            enforcement_kind="cgroup-v2" if cgroups is not None else "unboxed",
            runner_name="run",
        )
        metrics = csv_metrics
        profile_run_id = csv_metrics.run_id

    # --stress implies --keep-going: a failing copy must not cancel, nor prevent the launch of,
    # its siblings — or the per-copy ratio (the whole point) becomes a partial measurement.
    keep_going = bool(ns.keep_going) or stress_active
    result = run_dag_limited(
        cfg,
        max_steps=max_steps,
        max_cpus=max_cpus,
        cgroups=cgroups,
        metrics=metrics,
        keep_going=keep_going,
        verbosity=verbosity,
        order=list(plan.order),
        run_timeout_s=_effective_run_timeout(ns),
        profile_timeseries_interval_s=profile_timeseries_interval_s,
    )
    passed = sum(1 for o in result.outcomes if o.ok)
    failed = sum(1 for o in result.outcomes if not o.ok and not o.aborted)
    aborted = sum(1 for o in result.outcomes if o.aborted)
    verdict = c.green("PASS") if result.ok else c.red("FAIL")
    print(
        f"{PROG}: {verdict} - {passed} passed, {failed} failed, {aborted} aborted, "
        f"{len(result.intentional_skips)} intentionally skipped, "
        f"{len(result.skipped)} dependency-skipped, "
        f"{len(result.not_launched)} not launched in {result.wall_s:.1f}s",
        file=sys.stderr,
    )
    if result.not_launched:
        print(f"{PROG}: not launched: {', '.join(result.not_launched)}", file=sys.stderr)
    if perf_dir is not None:
        _report_profile_written(perf_dir, source)
    if do_upload and backend is not None:
        _sync_upload(backend, result.step_profile_rows, local_run_id=profile_run_id)
    # --stress: print the per-copy PASS/FAIL ratio (the finding) to stdout.
    if stress_active:
        _print_stress_report(
            result.outcomes,
            stress_n,
            result.max_concurrent_steps,
            max_steps,
            max_cpus,
            c,
        )
    # Feature C: --profile prints a per-step profile table to stdout.
    if bool(ns.profile):
        _print_profile_table(result.step_profile_rows, c)
    if core_reservation is not None:
        core_reservation.release()
    return 0 if result.ok else 1


def _main(argv: Sequence[str] | None = None) -> int:
    """Run the command-line application and return its process status."""
    parser = build_parser()
    raw = list(argv) if argv is not None else list(sys.argv[1:])
    # --cgroups has been REMOVED (cgroup-v2 boxing is ON by default). Fail LOUDLY rather than
    # silently accepting a dead flag; per-node resource limits are DAG fields (cpu_timeout,
    # memory, pids). Kept in parity with the Rust build's parser hard-error.
    if any(a == "--cgroups" or a.startswith("--cgroups=") for a in raw):
        print(
            f"{PROG}: error: --cgroups has been removed (cgroup-v2 boxing is ON by default); "
            "drop the flag. Per-node resource limits are DAG fields (cpu_timeout, memory, pids).",
            file=sys.stderr,
        )
        return 2
    ns = parser.parse_args(raw)
    c = Palette(_color_enabled(sys.stdout))

    if bool(ns.userguide):
        # Write the edition-specific embedded guide verbatim (no added/stripped newline).
        sys.stdout.write(_load_userguide())
        return 0

    command = ns.command if isinstance(ns.command, str) else None
    if command is None:
        parser.print_help()
        return 0
    if command == "quickstart":
        print(_quickstart(c))
        return 0
    if command == "capabilities":
        # Machine-readable enforcement manifest, DERIVED from the guard registry rather than
        # typed out: byte-identical to the Rust build and cross-checked, so an enforcement guard
        # in one build but not the other fails `cross`.
        print(enforcement_manifest())
        return 0
    if command == "box":
        return _cmd_box(ns, parser, c)
    if command == "pin-run":
        return _cmd_pin_run(ns)
    if command == "sweep":
        return _cmd_sweep(ns, c)
    if command == "plan":
        return _cmd_plan(ns)
    if command == "summary":
        return _cmd_summary(ns)

    dag_arg = ns.dag if isinstance(ns.dag, str) else None
    if dag_arg is None:
        print(f"{PROG}: {command}: --dag FILE is required", file=sys.stderr)
        return 2
    if command == "run":
        refusal = nested_run_refusal(bool(ns.allow_unwise_nest_dagruns))
        if refusal is not None:
            print(f"{PROG}: run: {refusal}", file=sys.stderr)
            return 2
    # A boxed run re-execs itself inside systemd. When the DAG comes from stdin, that re-exec must
    # happen BEFORE this process consumes the pipe; otherwise the child sees EOF and reports
    # invalid JSON. The in-scope child skips this block, reads stdin once, and _run performs the
    # normal observed-containment check. Explicit unboxed modes never re-exec and need no change.
    if (
        command == "run"
        and dag_arg == "-"
        and not bool(ns.allow_cgroup_failure)
        and not bool(getattr(ns, "unsafe_no_cgroups", False))
        and os.environ.get("DAGRUN_IN_SCOPE") != "1"
    ):
        _manager, code = _resolve_cgroup_manager(
            False,
            False,
            max_cpus=_select_max_cpus(ns),
            run_timeout_s=_effective_run_timeout(ns),
            max_mem_bytes=_requested_max_mem_bytes(getattr(ns, "max_mem", None)),
        )
        if code != 0:
            return code
    try:
        cfg = _load(dag_arg)
    except (OSError, DagJsonError) as exc:
        print(f"{PROG}: {exc}", file=sys.stderr)
        return 2

    if command in ("ascii", "dot"):
        labels_raw = getattr(ns, "labels", None)
        if labels_raw is not None:
            labels = _parse_tag_list(labels_raw)
            if not labels:
                print(
                    f"{PROG} {command}: error: --labels requires at least one label",
                    file=sys.stderr,
                )
                return 2
            try:
                cfg = _select_steps_by_labels(cfg, labels)
            except _SelectedError as exc:
                print(f"{PROG}: {exc}", file=sys.stderr)
                return 2
        grouped = getattr(ns, "group_by", None) is not None
        if command == "ascii":
            sys.stdout.write(to_ascii_groups(cfg) if grouped else to_ascii(cfg))
        else:
            sys.stdout.write(to_dot_groups(cfg) if grouped else to_dot(cfg))
        return 0
    if command == "list":
        print(_render_list(cfg, c))
        return 0
    if command == "json":
        print(dag_to_json(cfg))
        return 0
    if command == "yaml":
        # dag_to_yaml needs the declared PyYAML dependency; surface its absence as a clean
        # actionable message (exit 2) rather than letting DagJsonError escape as a traceback.
        try:
            text = dag_to_yaml(cfg)
        except DagJsonError as exc:
            print(f"{PROG}: {exc}", file=sys.stderr)
            return 2
        # dag_to_yaml already ends with a trailing newline.
        sys.stdout.write(text)
        return 0
    if command == "run":
        return _run(cfg, ns, c)
    parser.print_help()
    return 2


def main(argv: Sequence[str] | None = None) -> int:
    """Run one CLI invocation and restore any parent-owned delegated caller afterward."""
    from dagrun import cgroup as cg

    coordinator_depth = cg.nested_coordinator_depth()
    try:
        result = _main(argv)
    finally:
        # A normal command-line process exits next, but ``main`` is also a public in-process API.
        # The nested aggregate cap includes the coordinator while the invocation is active; do
        # not leak that cgroup membership (or its tighter limits) into its long-lived caller.
        restored = cg.restore_nested_coordinators_to(coordinator_depth)
    if not restored:
        print(
            f"{PROG}: ERROR: nested scheduler cgroup membership could not be restored; "
            "refusing to report a successful invocation",
            file=sys.stderr,
        )
        return 3
    return result


if __name__ == "__main__":
    raise SystemExit(main())
