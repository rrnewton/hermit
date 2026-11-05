"""Core DAG configuration, step, resource, and result types.

The module contains pure data and helpers; callers provide the graph and its resource
hints, then pass a :class:`DagConfig` to the scheduler.
"""

from __future__ import annotations

import math
import os
import re
import signal
from collections.abc import Container, Mapping, Sequence
from dataclasses import dataclass, field, replace
from dataclasses import fields as dataclass_fields
from enum import Enum

#: Wall-clock backstop (seconds) for a step that declares NO wall budget AND no CPU budget to
#: derive one from. Wall time is LOAD-DEPENDENT, so it is only a defence-in-depth hang backstop;
#: the CPU-time budget is the real, load-immune guard.
DEFAULT_STEP_TIMEOUT = 1800

#: When a step declares a CPU-second budget but no wall budget, the wall backstop is derived at
#: this multiple of the (platform-scaled) CPU budget. A step legitimately spending C CPU-seconds
#: can take up to ~C wall-seconds when serialized on one core, plus scheduling slack under load;
#: 3x leaves generous headroom so the wall guard only ever fires on a true hang and never races
#: the authoritative CPU-second guard.
#:
#: That reasoning holds for CPU-BOUND work only, which is why :func:`resolved_wall_timeout` floors
#: the derived value at :data:`DEFAULT_STEP_TIMEOUT` and never uses it to tighten a step: a step
#: that blocks on the network or a lock spends almost no CPU and arbitrary wall time.
#:
#: The value and the name are DELIBERATELY the same as
#: ``parallel_experiment_runner.model.WALL_CPU_BACKSTOP_FACTOR``, which established this idiom in
#: this repository. One policy, spelled once per project rather than invented twice;
#: ``test_wall_timeout_derivation.py`` asserts the two constants are equal so they cannot drift.
WALL_CPU_BACKSTOP_FACTOR = 3

#: Deliberately SMALL default caps for a step that DECLARES NOTHING — the "forcing function".
#: An undeclared step is boxed into a tight 1-core / 1-GiB / 10-s-CPU floor, so a real step
#: immediately hits the cap and must DECLARE its true needs. That generates per-node resource
#: metadata EMPIRICALLY (from measured breaches) instead of by guessing. Each default applies
#: ONLY when the step leaves the matching hint unset; an explicit hint always wins. They are
#: DagConfig fields (below), so any caller can override or disable a dimension.
DEFAULT_SMALL_MEM_CAP_BYTES = 1024**3  # 1 GiB inner memory.max when no memory hint is declared
DEFAULT_SMALL_CPU_COUNT = 1  # 1-core cpu.max when no inner-parallelism width is declared
DEFAULT_SMALL_CPU_TIMEOUT = 10  # 10 s CPU-time budget when cpu_timeout is unset

#: Per-platform CPU-budget multiplier, applied at EXECUTION time to whatever CPU budget is in
#: effect for a step. A CPU second is load-immune (wall = cpu_busy + wait; contention inflates
#: only wait) but it is NOT clock-immune: a slower core retires the same instruction stream over
#: more seconds of CPU occupancy, so identical work legitimately burns more CPU-seconds on an
#: underpowered runner. A graph therefore carries ONE canonical `cpu_timeout` per step and the
#: platform scales it here.
#:
#: Applying it at execution — rather than baking a second column of pre-multiplied numbers into
#: the graph — is the whole point: two independently-maintained timeout tables drift, and a step
#: has only one `cpu_timeout` field, so a per-platform column would force declaration authors to
#: pick a single number that is either too tight for the slow platform or too loose for the fast
#: one (hiding the very hangs the budget exists to catch).
#:
#: 1.0 is a strict no-op: unset, every platform enforces the canonical budget exactly as before.
#: A platform opts in explicitly (`--cpu-timeout-multiplier`, or $DAGRUN_CPU_TIMEOUT_MULTIPLIER
#: in a lane's environment), and every breach message then states the canonical budget, the
#: multiplier and the platform label, so a kill stays attributable to a specific policy rather
#: than to an anonymous number.
DEFAULT_CPU_TIMEOUT_MULTIPLIER = 1.0

#: Environment override for :data:`DEFAULT_CPU_TIMEOUT_MULTIPLIER`, so a CI lane can set the
#: policy once for its whole platform without threading a flag through every invocation.
CPU_TIMEOUT_MULTIPLIER_ENV = "DAGRUN_CPU_TIMEOUT_MULTIPLIER"

#: Companion label naming the platform the multiplier describes. Free-form (e.g.
#: "github-hosted"); it appears verbatim in the breach message so the reader can find the lane
#: that set it. Empty when the multiplier is 1.0 or the caller supplied no label.
CPU_TIMEOUT_PLATFORM_ENV = "DAGRUN_CPU_TIMEOUT_PLATFORM"

#: Default template for the inner-parallelism (concurrency) flag appended to a step's command
#: when the step declares ``preferred_inner_jobs``. See :func:`render_jobs_flag`.
DEFAULT_JOBS_FLAG = "-j"

#: MACHINE-LEVEL name of the environment variable through which this host delivers a step's
#: inner width, for guests that read their worker count from the environment rather than argv
#: (cargo's ``CARGO_BUILD_JOBS`` is the motivating case).
#:
#: WHY THIS IS A MACHINE KNOB AND NOT A GRAPH FIELD. How many workers a step should use is a
#: property of the HOST; *which channel* a tool listens on is a property of the toolchain
#: INSTALLED on that host. Neither is a property of the work, so neither belongs in a DAG. This
#: mirrors :data:`CPU_TIMEOUT_PLATFORM_ENV`, which already scales a graph-declared budget per
#: platform without editing the graph.
#:
#: It exists because an appended flag cannot reach every guest: a step whose command is a
#: compound ``A && B`` would have the flag land only on ``B``, and a command ending in ``--``
#: would pass it through to a program that has no such option. Those steps carry an empty
#: ``jobs_flag`` today and are consequently unresizable, which makes the runner refuse them on
#: any host too small for their declared width.
JOBS_ENV_ENV = "DAGRUN_JOBS_ENV"

#: Shell variable through which a known ``cmdtype`` exposes its width arguments inside a
#: compound command. It is deliberately absent for ``unknown``.
DAGRUN_EXTRA_ARGS_ENV = "DAGRUN_EXTRA_ARGS"


class CmdType(Enum):
    """Known command-line shapes for runner-controlled inner parallelism."""

    UNKNOWN = "unknown"
    MAKE = "make"
    CARGO_BUILD = "cargo-build"
    CARGO_TEST = "cargo-test"
    CARGO_NEXTEST = "cargo-nextest"
    GENERIC_DASH_J_COMMAND = "generic-dash-j-command"
    GENERIC_WITH_FLAG = "generic-with-flag"


class StepClass(Enum):
    """How a step uses the machine, used for scheduling decisions."""

    CPU_BOUND = "cpu-bound"
    LATENCY_BOUND = "latency-bound"
    LIGHT = "light"


class IntentionalSkipReason(Enum):
    """Closed vocabulary for nodes deliberately omitted before process spawn."""

    EMPTY_MANIFEST_BUCKET = "empty-manifest-bucket"


class WriteDomainGuarantee(Enum):
    """Why concurrent writes declared by a step are safe.

    These are deliberately not scheduler resources.  In particular,
    ``artifact-barrier-dependent`` writers may run in parallel after consumers
    have been shielded behind an immutable artifact barrier, while
    ``explicitly-isolated`` writers use package/path-disjoint output.  Collapsing
    either case into one mutex would be correct but operationally unusable.
    """

    ARTIFACT_BARRIER_DEPENDENT = "artifact-barrier-dependent"
    IMMUTABLE_ARTIFACT_BARRIER = "immutable-artifact-barrier"
    EXPLICITLY_ISOLATED = "explicitly-isolated"
    ARTIFACT_PRODUCER = "artifact-producer"


# BARE CARGO IS SHIELDED, NOT SERIALIZED. External writers never enter this
# scheduler, so publication barriers protect consumers without inventing a lock.


@dataclass(frozen=True)
class WriteDomainPolicy:
    """Closed write-domain vocabulary and omission policy for one DAG."""

    require_explicit: bool = False
    allowed_domains: frozenset[str] = field(default_factory=frozenset)


@dataclass(frozen=True)
class ResourceHint:
    """Optional per-step resource demand, duration, parallelism, and memory hints.

    Estimates enable memory-aware concurrency and longest-processing-time dispatch;
    scarce-resource demands constrain which steps may run together.
    """

    # Scarce-resource DEMAND for this step, e.g. {"browser": 1}. The runner never lets the
    # summed demand of concurrently-running steps exceed DagConfig.resource_caps.
    resources: Mapping[str, int] = field(default_factory=dict)
    # Estimated wall-clock seconds, used only to order ready steps (longest first). 0 sorts
    # last; stale values only mildly degrade packing and are never a correctness contract.
    est_duration_s: float = 0.0
    # Estimated peak resident memory (bytes). None excludes the step from the memory model.
    rss_baseline_bytes: int | None = None
    # Width at which ``rss_baseline_bytes`` was measured exactly. This is planner-derived,
    # transient provenance (authored DAG readers/writers deliberately do not expose it): when set,
    # the memory model must not apply the legacy width heuristic again at that same width.
    rss_baseline_inner_jobs: int | None = None
    # Explicit hard per-step memory cap (bytes); overrides the derived cap when set.
    hard_mem_max_bytes: int | None = None
    classification: StepClass = StepClass.LIGHT
    # Internal parallelism width for the step's own command (e.g. a build's -j). None means
    # "not measured/declared".
    preferred_inner_jobs: int | None = None
    measured_effective_cores: float | None = None
    measured_cpu_utilization: float | None = None


@dataclass(frozen=True)
class DagManifest:
    """The manifest cell population selected by one DAG step."""

    lane: str
    category: str
    test: str | None = None
    mode: str | None = None
    backend: str | None = None

    def matches_exact_result(self, result: DagManifest) -> bool:
        """Whether this selector matches one exact manifest-result identity."""

        return (
            self.lane == result.lane
            and self.category == result.category
            and (self.test is None or self.test == result.test)
            and (self.mode is None or self.mode == result.mode)
            and (self.backend is None or self.backend == result.backend)
        )

    def exact_identity(self) -> str:
        """Render an exact identity or refuse an incomplete result."""

        missing = [
            name
            for name, value in (
                ("lane", self.lane),
                ("category", self.category),
                ("test", self.test),
                ("mode", self.mode),
                ("backend", self.backend),
            )
            if not value
        ]
        if missing:
            raise ValueError(
                "result manifest is not an exact identity: missing " + ", ".join(missing)
            )
        return f"{self.lane}/{self.category}/{self.test}/{self.mode}/{self.backend}"


STRUCTURED_TEST_RESULTS_KIND = "structured-test-results"
STRUCTURED_TEST_RESULTS_CURRENT_SCHEMA = 2
STRUCTURED_TEST_RESULTS_RETAINED_SCHEMA = 2
STRUCTURED_TEST_RESULTS_CLASSIFIED_SCHEMA = 3
STRUCTURED_TEST_RESULTS_DIAGNOSTIC_SCHEMA = 4
STRUCTURED_TEST_RESULTS_PATH_ENV = "DAGRUN_TEST_COUNTS_PATH"


@dataclass(frozen=True)
class StructuredTestResultsManifest:
    """One step's exact-schema structured test-result path declaration."""

    schema: int
    owner: str

    @classmethod
    def classified(cls, owner: str) -> "StructuredTestResultsManifest":
        """Explicitly require complete classified attempts (schema 3)."""
        return cls(schema=STRUCTURED_TEST_RESULTS_CLASSIFIED_SCHEMA, owner=owner)

    @classmethod
    def diagnostic(cls, owner: str) -> "StructuredTestResultsManifest":
        """Require classified attempts that may carry non-blocking diagnostic failures (schema 4)."""
        return cls(schema=STRUCTURED_TEST_RESULTS_DIAGNOSTIC_SCHEMA, owner=owner)

    @classmethod
    def current(cls, owner: str) -> "StructuredTestResultsManifest":
        """Declare results in this release line's current schema."""
        return cls(schema=STRUCTURED_TEST_RESULTS_CURRENT_SCHEMA, owner=owner)

    @classmethod
    def retained_schema2(cls, owner: str) -> "StructuredTestResultsManifest":
        """Temporarily declare retained schema-2 producer output."""
        return cls(schema=STRUCTURED_TEST_RESULTS_RETAINED_SCHEMA, owner=owner)


ResultManifest = DagManifest | StructuredTestResultsManifest


@dataclass
class Step:
    """One node in the DAG: a shell command plus its dependencies and resource hint."""

    group: str
    job: str
    desc: str
    cmd: str  # shell command (bash -c), run from the run's working directory
    # Optional long-form documentation for this node (default empty). Unlike `desc` — a short
    # label shown by `list`/`run` — `description` is free-form prose (often multi-line, e.g. a
    # YAML block scalar) that documents WHY the step exists. It never affects scheduling.
    description: str = ""
    # Selection labels are independent of the unique ``group.job`` tag. A step may belong to
    # several named subsets without changing either its identity or its dependency edges.
    labels: list[str] = field(default_factory=list)
    deps: list[str] = field(default_factory=list)  # tags ("group.job") this step depends on
    env: dict[str, str] = field(default_factory=dict)
    hint: ResourceHint = field(default_factory=ResourceHint)
    networkonly: bool = False  # skipped when networking is disabled
    engine_only: bool = False  # selected only by an engine-only subset preset
    # Permit this step to launch child dagrun schedulers without weakening the default nested-run
    # refusal.  Under cgroup boxing the scheduler gives the command an empty delegation root,
    # keeps the command itself in a supervisor leaf, and retains ownership of the whole subtree.
    delegated_children: bool = False
    # Wall-clock ceiling in seconds. 0 means UNDECLARED, not unlimited: the effective bound is
    # then derived (see :func:`resolved_wall_timeout`) from the step's CPU budget, or falls back
    # to DEFAULT_STEP_TIMEOUT. A hardcoded 1800 here was the load-sensitive number baked into
    # every graph that the derivation exists to remove.
    timeout: int = 0
    # CPU-time budget in seconds (user+system, measured from the step's cgroup
    # cpu.stat). 0 disables the CPU-time guard, leaving only the wall `timeout`.
    # Unlike wall time, CPU time is immune to machine load, so a CPU budget can be
    # set much tighter than a load-tolerant wall timeout without flaking. Enforced
    # only when cgroup boxing is active (cpu.stat available); otherwise inert.
    cpu_timeout: int = 0
    # Template for the inner-parallelism flag appended to `cmd` when this step declares
    # `preferred_inner_jobs`. None inherits DagConfig.default_jobs_flag; "" disables appending
    # (the step manages a fixed declared width, which the planner cannot resize). See
    # render_jobs_flag for the template forms.
    jobs_flag: str | None = None
    # Environment variable through which THIS step's guest accepts its worker count. None
    # inherits DagConfig.default_jobs_env (normally set from $DAGRUN_JOBS_ENV by the
    # host, not by the graph); "" disables the env channel for this step specifically.
    jobs_env: str | None = None
    # Known command-line shape for delivering the admitted width. ``unknown`` preserves the
    # existing jobs_flag/jobs_env behavior and never sets DAGRUN_EXTRA_ARGS.
    cmdtype: CmdType = CmdType.UNKNOWN
    # A typed, pre-execution omission. This is not PASS and is kept separate from
    # dependency-skipped nodes in RunResult. Unknown strings are rejected by the loader.
    skip_reason: IntentionalSkipReason | None = None
    # Presence is load-bearing: ``None`` means the field was not declared,
    # whereas ``[]`` explicitly declares no writes to the policy's protected
    # artifact domains.  An enabled
    # WriteDomainPolicy refuses the former before any node starts.
    write_domains: list[str] | None = None
    write_domain_guarantee: WriteDomainGuarantee | None = None
    # Tags ("group.job") whose FAILURE this step exists to explain.
    #
    # WHY THIS IS A RELATIONSHIP AND NOT A BOOLEAN. A diagnostic node -- one whose only job is
    # to name the cause of another node's failure -- is BY CONSTRUCTION scheduled alongside
    # something that fails, so eager-exit cancels it precisely when it was about to be useful.
    # Observed in a consuming graph: a test node failed a few seconds before its companion
    # ABI-comparison node, the companion was never launched, and the run reported the opaque
    # symptom while the node that would have named the missing symbol produced nothing.
    #
    # The obvious fix is a per-node "never cancel me" flag, and that is the wrong shape: anything
    # could set it, nothing would say why, and eager-exit would erode into an opt-out. Declaring
    # WHAT a node explains keeps the intent visible in the document, makes it CHECKABLE (the
    # loader refuses a tag that does not exist, a self-reference, and a cycle), and -- most
    # importantly -- lets the exemption be CONDITIONAL rather than blanket: see
    # :meth:`Step.explains_a_failure_in`. A declared explainer is still cancelled normally when
    # something it does not explain fails.
    explains: list[str] = field(default_factory=list)
    # Steps carrying the same non-empty value form one fail-fast family. A failure cancels only
    # running and queued peers in that family; true dependents remain excluded by the ordinary
    # dependency closure, while independent families continue. None preserves the existing
    # global eager-exit behavior, so existing graphs do not silently become keep-going runs.
    fail_fast_family: str | None = None
    # Typed manifest selection carried by a manifest bucket step. Consumers use this value
    # instead of reconstructing the lane and category from ``cmd``. Kept after the established
    # positional fields so existing programmatic Step(...) calls retain their argument meaning.
    manifest: DagManifest | None = None
    # Exact Cargo integration-test binary targets executed by this step. ``None`` preserves
    # historical graphs; a present list is checked against the command by the registration audit.
    integration_test_binaries: list[str] | None = None
    # Typed result declarations produced by this step. ``None`` preserves the existing singular
    # manifest-cell fallback; ``[]`` explicitly declares that this step produces no results.
    # Structured test evidence is not represented by a fabricated manifest cell.
    # Kept after every established field so existing positional Step(...) calls stay compatible.
    result_manifests: list[ResultManifest] | None = None

    def explains_a_failure_in(self, failed: Container[str]) -> bool:
        """Whether this step is exempt from eager-exit given the set of tags that FAILED.

        The exemption is deliberately narrow. Declaring ``explains`` does not make a step
        immortal; it only protects the step when one of the specific nodes it claims to explain
        has actually failed. A diagnostic that explains nothing about THIS failure is reaped like
        any other peer, so eager-exit keeps doing its job everywhere else.
        """
        return any(tag in failed for tag in self.explains)

    @property
    def tag(self) -> str:
        """Return the stable ``group.job`` identifier for this step."""
        return f"{self.group}.{self.job}"

    def effective_result_manifests(self) -> tuple[DagManifest, ...]:
        """Return authoritative selectors, falling back to the singular declaration."""

        if self.result_manifests is not None:
            return tuple(
                manifest
                for manifest in self.result_manifests
                if isinstance(manifest, DagManifest)
            )
        if self.manifest is not None:
            return (self.manifest,)
        return ()


    def structured_test_results_manifest(
        self,
    ) -> StructuredTestResultsManifest | None:
        """Return one valid structured-result declaration or refuse ambiguity."""

        structured = [
            manifest
            for manifest in self.result_manifests or ()
            if isinstance(manifest, StructuredTestResultsManifest)
        ]
        if len(structured) > 1:
            raise ValueError(
                f"step {self.tag}: declares structured test results more than once"
            )
        if structured and structured[0].schema not in {
            STRUCTURED_TEST_RESULTS_RETAINED_SCHEMA,
            STRUCTURED_TEST_RESULTS_CURRENT_SCHEMA,
            STRUCTURED_TEST_RESULTS_CLASSIFIED_SCHEMA,
            STRUCTURED_TEST_RESULTS_DIAGNOSTIC_SCHEMA,
        }:
            raise ValueError(
                f"step {self.tag}: structured test-result schema {structured[0].schema} is unsupported"
            )
        if structured and structured[0].owner != self.tag:
            raise ValueError(
                f"step {self.tag}: structured test-result owner {structured[0].owner!r} "
                "must equal the containing step tag"
            )
        return structured[0] if structured else None

def result_manifest_owner(steps: Sequence[Step], result: DagManifest) -> Step:
    """Resolve one exact manifest result to its unique owner among selected steps."""

    identity = result.exact_identity()
    owners = [
        step
        for step in steps
        if any(
            selector.matches_exact_result(result)
            for selector in step.effective_result_manifests()
        )
    ]
    if not owners:
        raise ValueError(
            f"result manifest {identity} has no owning step in the selected DAG"
        )
    if len(owners) > 1:
        raise ValueError(
            f"result manifest {identity} has multiple owning steps in the selected DAG: "
            + ", ".join(step.tag for step in owners)
        )
    return owners[0]


def render_jobs_flag(template: str, inner_jobs: int) -> str:
    """Render an inner-parallelism (concurrency) flag from a template and a job count.

    Three forms let a caller match any tool's flag spelling:

    * template contains ``%d`` -> substitute (full control, no auto-space):
      ``"-j%d"`` -> ``"-j4"``, ``"--num-threads=%d"`` -> ``"--num-threads=4"``.
    * template ends with ``=`` -> concatenate (no space): ``"--jobs="`` -> ``"--jobs=4"``.
    * otherwise -> space-separated: ``"--num-threads"`` -> ``"--num-threads 4"``, and the
      default ``"-j"`` -> ``"-j 4"``.
    """
    if "%d" in template:
        return template.replace("%d", str(inner_jobs))
    if template.endswith("="):
        return f"{template}{inner_jobs}"
    return f"{template} {inner_jobs}"


def effective_jobs_flag(step: Step, default_jobs_flag: str) -> str:
    """The jobs-flag template in effect for a step: its own ``jobs_flag`` overrides the
    DagConfig-level default; ``None`` inherits the default."""
    return step.jobs_flag if step.jobs_flag is not None else default_jobs_flag


_ENV_NAME_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
_RUNNER_ENV_NAMES = frozenset(
    {
        "DAGRUN_DELEGATED_CGROUP",
        "DAGRUN_DELEGATED_UNBOXED",
        "DAGRUN_EXTRA_ARGS",
        "DAGRUN_OUTER_RUN",
        "DAGRUN_STEP",
    }
)


# These assignable names change shell startup/control rather than carrying a worker count.
# Dynamic/readonly parameters are handled by the final shell capability probe.
_SHELL_CONTROL_JOBS_ENV_NAMES = frozenset({
    "BASH_COMPAT", "BASH_ENV", "BASH_XTRACEFD", "CDPATH", "ENV",
    "EXECIGNORE", "GLOBIGNORE", "PATH", "POSIXLY_CORRECT",
})


def resolve_jobs_env(
    explicit: str | None = None,
    env: Mapping[str, str] | None = None,
) -> str:
    """Resolve this machine's inner-width ENV channel name.

    Precedence: an explicit caller value wins over $DAGRUN_JOBS_ENV, which wins over
    "" (this machine offers no env channel).

    A MALFORMED NAME IS REFUSED rather than silently ignored, for the same reason
    :func:`resolve_cpu_timeout_multiplier` refuses one: a typo that quietly disabled the channel
    would turn every self-managed step back into a hard refusal on a small host, and the operator
    would see a capacity error rather than the configuration mistake that caused it.
    """
    environ = os.environ if env is None else env
    raw = (explicit if explicit is not None else environ.get(JOBS_ENV_ENV) or "").strip()
    if not raw:
        return ""
    if not _ENV_NAME_RE.fullmatch(raw):
        raise ValueError(
            f"{JOBS_ENV_ENV}={raw!r} is not a valid environment variable name"
        )
    if raw in _RUNNER_ENV_NAMES:
        raise ValueError(
            f"{JOBS_ENV_ENV}={raw!r} is reserved by dagrun and cannot carry a step's worker count"
        )
    if raw in _SHELL_CONTROL_JOBS_ENV_NAMES:
        raise ValueError(
            f"{JOBS_ENV_ENV}={raw!r} is a shell startup/control variable, not a safe "
            "jobs environment channel"
        )
    return raw


def effective_jobs_env(step: Step, default_jobs_env: str) -> str:
    """The inner-width ENV channel in effect for a step: its own ``jobs_env`` overrides the
    DagConfig-level default; ``None`` inherits the default.

    The returned name is normalized and validated even for programmatically-built configs. A
    malformed channel must fail before execution rather than making an env-only step look
    resizable and then failing (or setting the wrong variable) in a worker thread.
    """
    raw = step.jobs_env if step.jobs_env is not None else default_jobs_env
    return resolve_jobs_env(raw, env={})


def validate_jobs_env_config(cfg: DagConfig) -> None:
    """Validate every configured inner-width ENV channel before any DAG node may spawn."""
    resolve_jobs_env(cfg.default_jobs_env, env={})
    for step in cfg.steps:
        if step.jobs_env is not None:
            resolve_jobs_env(step.jobs_env, env={})


def env_with_inner_jobs(
    step: Step, default_jobs_env: str, inner_jobs: int | None
) -> dict[str, str]:
    """The environment overlay carrying a step's inner width, when this machine offers a channel.

    Returns ``{}`` when the step declares no width or no channel is configured, so a host that
    sets nothing behaves exactly as before. The value is the SAME width the flag path would
    render, so the two channels cannot disagree.
    """
    if inner_jobs is None:
        return {}
    name = effective_jobs_env(step, default_jobs_env).strip()
    if not name:
        return {}
    return {name: str(inner_jobs)}


def step_width_is_resizable(step: Step, default_jobs_flag: str, default_jobs_env: str) -> bool:
    """Can the runner actually lower this step's declared inner width?

    True when a known cmdtype or either existing channel is available. This is the predicate the
    capacity refusal keys on: a step with no way to receive the changed width bakes it into its
    own command, so clamping its cgroup quota alone would leave the original worker count running
    inside a smaller box -- a slowdown disguised as a limit.
    """
    # Resolve the env channel first so a valid jobs flag cannot short-circuit validation of a
    # malformed programmatic jobs_env value.
    jobs_env = effective_jobs_env(step, default_jobs_env)
    if step.cmdtype is not CmdType.UNKNOWN:
        return True
    return bool(effective_jobs_flag(step, default_jobs_flag).strip() or jobs_env.strip())


def command_with_inner_jobs(
    step: Step, default_jobs_flag: str, inner_jobs: int | None
) -> str:
    """The step's shell command with its inner-parallelism arguments, when applicable.

    A known cmdtype supplies its own spelling and leaves a command containing
    ``$DAGRUN_EXTRA_ARGS`` or ``${DAGRUN_EXTRA_ARGS}`` unchanged. ``unknown`` retains the existing
    effective jobs-flag behavior. A missing width leaves every command unchanged.
    """
    if step.cmdtype is not CmdType.UNKNOWN:
        extra_args = cmdtype_extra_args(step, inner_jobs)
        if extra_args is None or command_uses_extra_args(step.cmd):
            return step.cmd
        return f"{step.cmd} {extra_args}"
    if inner_jobs is None:
        return step.cmd
    template = effective_jobs_flag(step, default_jobs_flag)
    if not template.strip():
        return step.cmd
    return f"{step.cmd} {render_jobs_flag(template, inner_jobs)}"


def command_uses_extra_args(command: str) -> bool:
    """Whether a command contains either documented DAGRUN_EXTRA_ARGS expansion."""
    present, _single_quoted, _double_quoted = _extra_args_references(command)
    return present


def _extra_args_references(command: str) -> tuple[bool, bool, bool]:
    """Return presence plus whether a reference occurs inside single or double quotes."""
    present = single_quoted = double_quoted = False
    quote = ""
    index = 0
    while index < len(command):
        char = command[index]
        if quote != "'" and char == "\\":
            index += 2
            continue
        if char in "'\"":
            if not quote:
                quote = char
            elif quote == char:
                quote = ""
            index += 1
            continue
        matched = 0
        if command.startswith("${DAGRUN_EXTRA_ARGS}", index):
            matched = len("${DAGRUN_EXTRA_ARGS}")
        elif command.startswith("$DAGRUN_EXTRA_ARGS", index):
            end = index + len("$DAGRUN_EXTRA_ARGS")
            if end == len(command) or not (command[end].isalnum() or command[end] == "_"):
                matched = len("$DAGRUN_EXTRA_ARGS")
        if matched:
            present = True
            single_quoted = single_quoted or quote == "'"
            double_quoted = double_quoted or quote == '"'
            index += matched
            continue
        index += 1
    return present, single_quoted, double_quoted


def _command_is_compound(command: str) -> bool:
    quote = ""
    index = 0
    while index < len(command):
        char = command[index]
        if quote != "'" and char == "\\":
            index += 2
            continue
        if char in "'\"":
            if not quote:
                quote = char
            elif quote == char:
                quote = ""
            index += 1
            continue
        if not quote and (
            char in ";\n" or command.startswith("&&", index) or char == "|"
        ):
            return True
        index += 1
    return False


def cmdtype_extra_args(step: Step, inner_jobs: int | None) -> str | None:
    """Width arguments supplied by a known cmdtype, or ``None`` when none apply."""
    if inner_jobs is None or step.cmdtype is CmdType.UNKNOWN:
        return None
    if step.cmdtype in (CmdType.MAKE, CmdType.GENERIC_DASH_J_COMMAND):
        return f"-j{inner_jobs}"
    if step.cmdtype in (CmdType.CARGO_BUILD, CmdType.CARGO_TEST):
        return f"--jobs {inner_jobs}"
    if step.cmdtype is CmdType.CARGO_NEXTEST:
        return f"--test-threads {inner_jobs}"
    if step.cmdtype is CmdType.GENERIC_WITH_FLAG:
        assert step.jobs_flag is not None
        return render_jobs_flag(step.jobs_flag, inner_jobs)
    raise AssertionError(f"unhandled cmdtype {step.cmdtype.value}")


def cmdtype_env_with_inner_jobs(step: Step, inner_jobs: int | None) -> dict[str, str]:
    """DAGRUN_EXTRA_ARGS for a known cmdtype; unknown leaves the variable absent."""
    extra_args = cmdtype_extra_args(step, inner_jobs)
    return {} if extra_args is None else {DAGRUN_EXTRA_ARGS_ENV: extra_args}


def validate_cmdtype_config(cfg: DagConfig) -> None:
    """Refuse ambiguous cmdtype configuration and wrong multi-token quoting."""
    for step in cfg.steps:
        if step.cmdtype is CmdType.GENERIC_WITH_FLAG and (
            step.jobs_flag is None or not step.jobs_flag.strip()
        ):
            raise ValueError(
                f"step {step.tag}: cmdtype generic-with-flag requires a non-empty jobs_flag"
            )
        if (
            step.cmdtype not in (CmdType.UNKNOWN, CmdType.GENERIC_WITH_FLAG)
            and step.jobs_flag is not None
            and step.jobs_flag.strip()
        ):
            raise ValueError(
                f"step {step.tag}: jobs_flag is valid with cmdtype generic-with-flag, not "
                f"{step.cmdtype.value}"
            )
        extra_args = cmdtype_extra_args(step, 1)
        present, single_quoted, double_quoted = _extra_args_references(step.cmd)
        if extra_args is not None and _command_is_compound(step.cmd) and not present:
            raise ValueError(
                f"step {step.tag}: compound cmd with cmdtype {step.cmdtype.value} must place "
                "unquoted $DAGRUN_EXTRA_ARGS or ${DAGRUN_EXTRA_ARGS} where the width belongs"
            )
        if extra_args is not None and single_quoted:
            raise ValueError(
                f"step {step.tag}: DAGRUN_EXTRA_ARGS must not be single-quoted because the "
                "shell would not expand it"
            )
        if extra_args is not None and len(extra_args.split()) > 1 and double_quoted:
            raise ValueError(
                f"step {step.tag}: DAGRUN_EXTRA_ARGS must be unquoted for cmdtype "
                f"{step.cmdtype.value} because it contains multiple shell words; quoting it "
                "would pass one argument"
            )


def step_classification(step: Step) -> StepClass:
    """Return the explicit class, infer latency-bound browser work, or default to light."""
    if step.hint.classification is not StepClass.LIGHT:
        return step.hint.classification
    if "browser" in step.hint.resources:
        return StepClass.LATENCY_BOUND
    return StepClass.LIGHT


def preferred_inner_jobs(step: Step, experiment_override: int | None = None) -> int | None:
    """Positive internal parallelism width: an explicit override wins, else the hint.

    Zero/negative library-authored values mean undeclared and fall through to the configured
    per-step CPU default rather than becoming an invalid command flag or declared guest width.
    """
    value = experiment_override if experiment_override is not None else step.hint.preferred_inner_jobs
    return value if (value is not None and value > 0) else None


def canonical_cpu_timeout(step: Step, default_cpu_timeout: int) -> int:
    """CANONICAL CPU-time budget (seconds) for a step, before any per-platform scaling: its
    declared ``cpu_timeout`` (>0) wins; otherwise the DAG's SMALL default. Both 0 means the guard
    is disabled. This is the forcing-function default for the CPU-time dimension (see
    DEFAULT_SMALL_CPU_TIMEOUT). This is the number a graph declares and a derivation pipeline
    produces — one table, platform-independent."""
    return step.cpu_timeout if step.cpu_timeout > 0 else default_cpu_timeout


def scale_cpu_timeout(canonical: int, multiplier: float) -> int:
    """Apply a per-platform multiplier to a canonical CPU budget.

    Rounds to whole seconds (the enforcement poll is 1 Hz, so sub-second precision is not
    meaningful) and never rounds a live budget down to 0 — that would silently DISABLE the guard
    on a platform whose multiplier is small, turning a scaling policy into an opt-out. A disabled
    budget (canonical 0) stays disabled regardless of the multiplier.
    """
    if canonical <= 0:
        return 0
    if multiplier == DEFAULT_CPU_TIMEOUT_MULTIPLIER:
        return canonical
    # Round HALF AWAY FROM ZERO, not Python's banker's rounding. Two reasons, both
    # load-bearing: (1) Rust's f64::round() is half-away-from-zero, and a budget that differs
    # between the engines by a second is a real cross-language divergence (round(4.5) is 4 in
    # Python, 5 in Rust); (2) at a tie the more generous budget is the right default for a
    # guard whose whole purpose is to avoid false-killing a healthy-but-slow platform.
    return max(1, math.floor(canonical * multiplier + 0.5))


def effective_cpu_timeout(
    step: Step,
    default_cpu_timeout: int,
    multiplier: float = DEFAULT_CPU_TIMEOUT_MULTIPLIER,
) -> int:
    """CPU-time budget actually ENFORCED for a step on this platform: the canonical budget
    (:func:`canonical_cpu_timeout`) scaled by the platform multiplier
    (:func:`scale_cpu_timeout`). With the default 1.0 multiplier this is exactly the canonical
    budget, so an unconfigured platform behaves as it always did."""
    return scale_cpu_timeout(canonical_cpu_timeout(step, default_cpu_timeout), multiplier)


def resolved_wall_timeout(
    step: Step,
    default_step_timeout: int,
    multiplier: float = DEFAULT_CPU_TIMEOUT_MULTIPLIER,
) -> int:
    """The wall-clock ceiling a step is actually run under, deriving one when none was declared.

    Precedence, most specific first:

    1. the step's own ``timeout`` (>0) — an explicit author decision always wins;
    2. the document's ``default_step_timeout`` (>0) — an explicit document-wide decision;
    3. ``WALL_CPU_BACKSTOP_FACTOR`` x the step's PLATFORM-SCALED ``cpu_timeout``, when the step
       DECLARED one AND that is LARGER than :data:`DEFAULT_STEP_TIMEOUT`;
    4. :data:`DEFAULT_STEP_TIMEOUT`.

    *THE DERIVATION ONLY EVER LOOSENS.* Rule 3 is floored at :data:`DEFAULT_STEP_TIMEOUT`, so no
    step that ran under 1800 s before this rule existed runs under less now. Without that floor
    the rule silently retimed every already-authored step that declared a CPU budget: a
    ``networkonly`` step ``{"cmd": "git fetch ...", "cpu_timeout": 5}`` burns ~5 CPU-seconds and
    blocks for minutes on the network, and a 15-second ceiling SIGTERMs it and reports a hang.
    Wall time is unbounded relative to CPU time for anything that blocks, so a CPU-derived
    ceiling is only sound as an UPPER bound. The direction the derivation is for is the other
    one: a step declaring ``cpu_timeout: 900`` had a 1800 s wall ceiling that its own CPU guard
    could reach — at a 2.5x platform multiplier the enforced budget is 2250 s, ABOVE the wall
    bound — so the wall guard fired first and reported a hang where the truth was a slow machine.
    Rule 3 lifts that step to 2700 s and restores the 3x margin.

    Two further choices in rule 3 are deliberate and were the open questions in the design:

    *DECLARED, not canonical.* :func:`canonical_cpu_timeout` fills in the DAG's small default
    (10 s) for a step that declares nothing, and it is ALWAYS in force. Deriving from that would
    hand every undeclared step a 30-second wall ceiling where it currently gets 1800 — a silent,
    enormous tightening applied to exactly the steps whose needs nobody has measured yet. So the
    derivation fires only for a step whose author stated a CPU budget, and everything else falls
    to rule 4 with the behaviour it has always had.

    *SCALED, not canonical.* ``cpu_timeout_multiplier`` exists to loosen the CPU guard on a slow
    platform. A wall backstop pinned to the unscaled number would shrink to 3/multiplier of the
    enforced budget and start racing — firing FIRST on precisely the platform the multiplier was
    added for, and reporting a wall hang where the truth is a slow machine. Tracking the scaled
    budget keeps the 3x ratio wherever the multiplier goes.
    """
    if step.timeout > 0:
        return step.timeout
    if default_step_timeout > 0:
        return default_step_timeout
    if step.cpu_timeout > 0:
        derived = WALL_CPU_BACKSTOP_FACTOR * scale_cpu_timeout(step.cpu_timeout, multiplier)
        return max(derived, DEFAULT_STEP_TIMEOUT)
    return DEFAULT_STEP_TIMEOUT


def resolve_cpu_timeout_multiplier(
    explicit: float | None = None,
    env: Mapping[str, str] | None = None,
) -> tuple[float, str]:
    """Resolve the platform CPU-budget multiplier and its platform label.

    Precedence: an explicit CLI value wins over the environment, which wins over the 1.0 no-op.
    A malformed or non-positive environment value is REFUSED rather than silently ignored — a
    typo that quietly reverted the platform to 1.0 would loosen enforcement invisibly, which is
    the failure mode this whole mechanism exists to prevent.
    """
    environ = os.environ if env is None else env
    label = (environ.get(CPU_TIMEOUT_PLATFORM_ENV) or "").strip()
    if explicit is not None:
        if explicit <= 0:
            raise ValueError(f"cpu-timeout multiplier must be > 0, got {explicit}")
        return explicit, label
    raw = (environ.get(CPU_TIMEOUT_MULTIPLIER_ENV) or "").strip()
    if not raw:
        return DEFAULT_CPU_TIMEOUT_MULTIPLIER, label
    try:
        value = float(raw)
    except ValueError as exc:
        raise ValueError(
            f"{CPU_TIMEOUT_MULTIPLIER_ENV}={raw!r} is not a number"
        ) from exc
    if value <= 0:
        raise ValueError(f"{CPU_TIMEOUT_MULTIPLIER_ENV}={raw!r} must be > 0")
    return value, label


def effective_cpu_count(step: Step, default_cpu_count: int | None) -> int | None:
    """Core cap (cgroup ``cpu.max``) in effect for a step: its declared ``preferred_inner_jobs``
    wins; otherwise the DAG's SMALL default. Bounds ONLY the cgroup cpu.max, never the command's
    inner ``-j`` flag (which stays keyed to the declared width, so an undeclared step gets a
    1-core box without a bogus ``-j 1`` appended to a command that may not accept it)."""
    inner = preferred_inner_jobs(step)
    if inner is not None:
        return inner
    return default_cpu_count if (default_cpu_count is not None and default_cpu_count > 0) else None


def _cpu_timeout_policy_suffix(
    canonical: int, multiplier: float, platform: str
) -> str:
    """`` (canonical 3s x2 github-hosted)`` when a platform multiplier scaled the budget, else
    empty. Silent at 1.0 so the overwhelmingly common unscaled message is unchanged."""
    if multiplier == DEFAULT_CPU_TIMEOUT_MULTIPLIER or canonical <= 0:
        return ""
    rendered = f"{multiplier:g}"
    label = f" {platform}" if platform else ""
    return f" (canonical {canonical}s x{rendered}{label})"


#: Cancelled because the whole run exhausted its outer budget, not by any failure.
#:
#: Kept beside its sibling below so the two cancellations cannot drift into wording that reads
#: the same. The clause after the comma is the part that matters to a reader deciding whether to
#: go looking for a failing step.
ABORTED_BY_RUN_BUDGET_REASON = (
    "ABORTED (cut short by the OUTER run budget, not by a failure of its own or of a peer)"
)

#: Cancelled by eager-exit after a peer step failed.
ABORTED_BY_PEER_FAILURE_REASON = (
    "ABORTED (eager-exit after another step failed; --keep-going would continue independent work)"
)


def step_failure_reason(
    *,
    returncode: int | None,
    oomed: bool,
    oom_kills: int,
    timed_out: bool,
    timeout: int,
    pids_guard_tripped: bool,
    pids_guard_reason: str | None,
    detail_write_failure: Sequence[str],
    cpu_timed_out: bool = False,
    cpu_timeout: int = 0,
    cpu_timeout_canonical: int = 0,
    cpu_timeout_multiplier: float = DEFAULT_CPU_TIMEOUT_MULTIPLIER,
    cpu_timeout_platform: str = "",
) -> str:
    """Describe a failed step without conflating an external signal with an OOM.

    Failure causes use this precedence:
    OOM > CPU-timeout > timeout > pids-guard > detail-capture-failure > signal > exit code.

    CPU-timeout is reported ahead of the wall timeout because it is the more specific
    cause: when a CPU budget is exceeded the runner reaps the step, and the wall guard
    may also observe the resulting exit. Distinguishing them keeps the failure reason
    honest about which budget actually tripped.

    A negative ``returncode`` alone only says that the child received a Unix signal; it must not
    be reported as an OOM without a positive kernel ``memory.events`` OOM counter. Conversely, a
    delegated boundary can report ``oom`` while its descendant victim owns ``oom_kill``, so the
    zero-kill OOM wording remains explicit rather than degrading to an unexplained SIGKILL.
    """
    if oomed:
        if oom_kills == 0:
            return (
                "MEMORY CAP EXHAUSTED (kernel reported OOM at inner MemoryMax; "
                "no local oom_kill counter)"
            )
        return f"OOM-KILLED (hit inner MemoryMax; {oom_kills} oom_kill event(s))"
    if cpu_timed_out:
        # When a platform multiplier is in effect the enforced number is NOT the number written
        # in the graph, so the message must carry both plus the policy that connects them —
        # otherwise the reader cannot tell a genuine overrun from a mis-set platform policy, and
        # cannot find which knob to turn.
        return f"CPU-TIMEOUT >{cpu_timeout}s cpu" + _cpu_timeout_policy_suffix(
            cpu_timeout_canonical, cpu_timeout_multiplier, cpu_timeout_platform
        )
    if timed_out:
        return f"TIMEOUT >{timeout}s"
    if pids_guard_tripped:
        return f"PIDS GUARD ({pids_guard_reason})"
    if detail_write_failure:
        return f"DETAIL CAPTURE FAILED ({detail_write_failure[0]})"
    if returncode is not None and returncode < 0:
        try:
            signal_name = signal.Signals(-returncode).name
        except ValueError:
            signal_name = f"signal {-returncode}"
        return (
            f"received {signal_name} with no validate timeout, pids guard, "
            "or child-cgroup OOM recorded"
        )
    return f"exit {returncode}"


@dataclass(frozen=True)
class DagConfig:
    """A complete step graph plus scheduling and containment policy.

    ``resource_caps`` bounds concurrent scarce-resource demand. Memory and CPU policy
    fields have conservative defaults and may be overridden per workload.
    """

    steps: tuple[Step, ...]
    # Optional long-form documentation for the WHOLE DAG (default empty). Free-form prose
    # (often multi-line) describing the pipeline as a whole; never affects scheduling.
    description: str = ""
    resource_caps: Mapping[str, int] = field(default_factory=dict)
    # Multiplier from a step's measured RSS baseline to its inner memory cap (headroom).
    mem_cap_factor: float = 1.25
    # Lower bound (bytes) on the modeled worst-case footprint, so active-step sizing never
    # concludes "0 fits". Default 8 GiB.
    mem_cap_floor_bytes: int = 8 * 1024**3
    # Multiplier applied to the modeled peak to leave headroom. 1.0 = no inflation.
    outer_mem_safety_factor: float = 1.0
    # Document-wide wall budget for steps that omit their own. 0 means the document declared
    # none, so each step derives its own (see :func:`resolved_wall_timeout`).
    default_step_timeout: int = 0
    # Default inner-parallelism flag template for steps that don't set their own `jobs_flag`.
    default_jobs_flag: str = DEFAULT_JOBS_FLAG
    # Default inner-parallelism ENV channel for steps that don't set their own `jobs_env`.
    # When omitted by a loaded document, resolved from $DAGRUN_JOBS_ENV (see
    # resolve_jobs_env); callers may also set it explicitly. Empty means no env channel.
    default_jobs_env: str = ""
    # --- Deliberately SMALL default caps applied to a step that DECLARES NOTHING ---
    # The forcing function (see the module-level DEFAULT_SMALL_* constants): an undeclared step is
    # boxed into a tight floor so it must declare its real needs. These are active by default; the
    # declarations-first migration has supplied measured budgets for nodes that exceed the floor.
    # Each applies ONLY when the step leaves the matching hint unset (an explicit hint wins).
    # `--unsafe-no-cgroups` is the deliberately loud escape hatch for an unboxed run.
    default_step_mem_cap_bytes: int | None = DEFAULT_SMALL_MEM_CAP_BYTES
    default_step_cpu_count: int | None = DEFAULT_SMALL_CPU_COUNT
    default_step_cpu_timeout: int = DEFAULT_SMALL_CPU_TIMEOUT
    # Tags (``group.job``) whose FAILURE is a DECLARED known-failure: it is reported and named
    # loudly but does NOT flip the run's aggregate verdict, so one persistently-flaky node (e.g.
    # a host-dependent test) can't invalidate every other step's validate record. Derived from a
    # declared file (see :func:`dagrun.io.load_known_failures`); NEVER silent — the scheduler
    # names each excluded failure at runtime and the loader names what it loaded. A
    # non-allowlisted failure still fails the run, and an allowlisted step that PASSES is
    # unaffected (membership is consulted only on failure). Empty by default (fail-closed).
    known_failures: frozenset[str] = frozenset()

    # --- Per-platform CPU-budget scaling (see DEFAULT_CPU_TIMEOUT_MULTIPLIER) ---
    # Execution-time multiplier over whatever canonical CPU budget is in effect, so one graph
    # runs unchanged on a fast dev box and an underpowered hosted runner. NOT persisted with the
    # graph: this is caller/platform policy, not a property of the pipeline, and writing it into
    # the DAG file would recreate the per-platform table this mechanism replaces.
    cpu_timeout_multiplier: float = DEFAULT_CPU_TIMEOUT_MULTIPLIER
    # Free-form platform label reported alongside the multiplier in a breach message.
    cpu_timeout_platform: str = ""
    write_domain_policy: WriteDomainPolicy = field(default_factory=WriteDomainPolicy)

    def by_tag(self) -> dict[str, Step]:
        """Index configured steps by their stable tags."""
        return {step.tag: step for step in self.steps}

    def with_steps(self, steps: Sequence[Step]) -> DagConfig:
        """This configuration's POLICY carried forward onto a different step list.

        The safe replacement for ``DagConfig(steps=steps)``, which is how the dropped-field bug
        is written every time: every field the call does not name reverts to its default, and the
        reverted fields appear in no diff, no warning and no failure.  Take a lane's steps and
        call this on the config they came from, and the caps, timeouts and memory policy travel
        with them by construction.

        ``resource_caps`` is copied rather than aliased: the result is a configuration of its
        own, not one that changes when the original's cap table is mutated afterwards.
        """
        return replace(self, steps=tuple(steps), resource_caps=dict(self.resource_caps))


#: Every top-level :class:`DagConfig` field, in declaration order — the checklist
#: :func:`dag_config_carry_diff` walks.  :func:`_check_dag_config_fields` asserts this list is
#: exactly the dataclass's own field set, so a NEW field cannot join ``DagConfig`` without being
#: given a comparison here: that is the Python stand-in for the Rust edition's exhaustive
#: destructuring, which makes the same omission a compile error.
DAG_CONFIG_FIELDS: tuple[str, ...] = (
    "steps",
    "description",
    "resource_caps",
    "mem_cap_factor",
    "mem_cap_floor_bytes",
    "outer_mem_safety_factor",
    "default_step_timeout",
    "default_jobs_flag",
    "default_jobs_env",
    "default_step_mem_cap_bytes",
    "default_step_cpu_count",
    "default_step_cpu_timeout",
    "known_failures",
    "cpu_timeout_multiplier",
    "cpu_timeout_platform",
    "write_domain_policy",
)


def _check_dag_config_fields() -> list[str]:
    """Field names present on :class:`DagConfig` but absent from :data:`DAG_CONFIG_FIELDS`, or
    the reverse — empty when the written-down checklist is exactly the dataclass."""
    declared = {f.name for f in dataclass_fields(DagConfig)}
    listed = set(DAG_CONFIG_FIELDS)
    return sorted((declared - listed) | (listed - declared))


def _render_caps(caps: Mapping[str, int]) -> str:
    return "{" + ",".join(f"{k}={v}" for k, v in sorted(caps.items())) + "}"


def _render_opt_int(value: int | None) -> str:
    """``None`` renders as ``<absent>``, never as ``0``: ABSENT IS NOT ZERO here either, and a
    disabled default cap and a cap of zero are opposite instructions."""
    return "<absent>" if value is None else str(value)


def _render_policy(policy: WriteDomainPolicy) -> str:
    domains = ",".join(sorted(policy.allowed_domains))
    return f"require_explicit={policy.require_explicit} allowed=[{domains}]"


def dag_config_carry_diff(frm: DagConfig, to: DagConfig) -> list[str]:
    """Every top-level field whose value DIFFERS between two configurations, named with both
    values.

    A CARRY ASSERTION.  A consumer that loads a DAG file, keeps its steps and rebuilds the config
    silently substitutes a default for every field it did not name — a 600 s wall budget becomes
    1800 s, an 8 GiB floor becomes whatever the constant says, and NOTHING reports it.  A cap that
    silently becomes a default is indistinguishable from a cap someone chose, so the only way to
    know a config survived a round trip is to compare it, field by field, against the one it came
    from::

        assert dag_config_carry_diff(loaded, rebuilt) == []

    Deliberately NOT dataclass equality.  The comparison enumerates fields one at a time against
    :data:`DAG_CONFIG_FIELDS`, and :func:`_check_dag_config_fields` refuses a ``DagConfig`` field
    that is not on that list — so adding a field forces a decision here, which is exactly this
    bug's shape: a new field quietly defaulting at a call site nobody revisited.  Plain ``==``
    would start silently covering new fields and then, the first time one held ``float('nan')``,
    silently stop being an assertion at all.

    ``steps`` are compared by tag sequence, not deeply: this answers "did the POLICY survive", and
    a consumer that rebuilds a config is by construction keeping the same steps.
    """
    unaccounted = _check_dag_config_fields()
    if unaccounted:
        raise AssertionError(
            "DAG_CONFIG_FIELDS is out of step with DagConfig for "
            + ", ".join(unaccounted)
            + "; give each field a comparison in dag_config_carry_diff before listing it"
        )
    rendered: tuple[tuple[str, object, object], ...] = (
        ("steps", tuple(s.tag for s in frm.steps), tuple(s.tag for s in to.steps)),
        ("description", frm.description, to.description),
        ("resource_caps", _render_caps(frm.resource_caps), _render_caps(to.resource_caps)),
        # Rendered, not compared as floats: NaN != NaN would report an unchanged field as
        # dropped, and a report that fires on a config nobody touched is a report nobody reads.
        ("mem_cap_factor", repr(frm.mem_cap_factor), repr(to.mem_cap_factor)),
        ("mem_cap_floor_bytes", frm.mem_cap_floor_bytes, to.mem_cap_floor_bytes),
        (
            "outer_mem_safety_factor",
            repr(frm.outer_mem_safety_factor),
            repr(to.outer_mem_safety_factor),
        ),
        ("default_step_timeout", frm.default_step_timeout, to.default_step_timeout),
        ("default_jobs_flag", frm.default_jobs_flag, to.default_jobs_flag),
        ("default_jobs_env", frm.default_jobs_env, to.default_jobs_env),
        (
            "default_step_mem_cap_bytes",
            _render_opt_int(frm.default_step_mem_cap_bytes),
            _render_opt_int(to.default_step_mem_cap_bytes),
        ),
        (
            "default_step_cpu_count",
            _render_opt_int(frm.default_step_cpu_count),
            _render_opt_int(to.default_step_cpu_count),
        ),
        ("default_step_cpu_timeout", frm.default_step_cpu_timeout, to.default_step_cpu_timeout),
        ("known_failures", tuple(sorted(frm.known_failures)), tuple(sorted(to.known_failures))),
        ("cpu_timeout_multiplier", repr(frm.cpu_timeout_multiplier), repr(to.cpu_timeout_multiplier)),
        ("cpu_timeout_platform", frm.cpu_timeout_platform, to.cpu_timeout_platform),
        (
            "write_domain_policy",
            _render_policy(frm.write_domain_policy),
            _render_policy(to.write_domain_policy),
        ),
    )
    if tuple(name for name, _, _ in rendered) != DAG_CONFIG_FIELDS:
        raise AssertionError(
            "dag_config_carry_diff compares fields in a different order/set than "
            "DAG_CONFIG_FIELDS declares"
        )
    return [
        f"{name}: {_flat(a)} -> {_flat(b)}" for name, a, b in rendered if a != b
    ]


def _flat(value: object) -> str:
    """Render one side of a carry difference on a single line."""
    if isinstance(value, tuple):
        return ",".join(str(v) for v in value)
    return str(value)


def undeclared_resource_demands(cfg: DagConfig) -> list[str]:
    """Steps demanding a named resource that ``resource_caps`` never declares.

    ABSENT IS NOT ZERO, and this is the one place the difference can still be seen.  The
    scheduler's gate reads ``resource_avail.get(name, 0)``, so an undeclared resource and a
    resource deliberately capped at 0 collapse into the same integer and produce byte-identical
    behaviour: the step is never ready, the ready-set loop keeps sleeping, and the run sits at 0%
    CPU emitting nothing until some outer deadline kills it.  Their remedies are opposites —
    "declare the capacity you forgot" versus "this is blocked on purpose" — so a report that
    cannot tell them apart is worse than no report.

    Only a demand GREATER THAN ZERO can starve, and an intentionally-skipped step never launches,
    so neither is named here.  A cap DECLARED as 0 is a real value and is likewise not named: it
    still gates the step, exactly as its author asked.

    Returns ``"<tag>: <resource>"`` entries, sorted, empty when every demand has a declared cap.
    """

    return sorted(
        {
            f"{step.tag}: {name}"
            for step in cfg.steps
            if step.skip_reason is None
            for name, count in step.hint.resources.items()
            if count > 0 and name not in cfg.resource_caps
        }
    )


def graph_structure_violations(cfg: DagConfig) -> list[str]:
    """Ways the GRAPH ITSELF cannot mean what it says, named before anything runs.

    Each entry describes a graph whose declared steps cannot all be executed in the order the
    graph asks for.  None of them is a matter of taste.

    DUPLICATE TAG is the worst of the set, because it is the one that stays SILENT.  Two steps
    declared with the same ``group.job`` collapse into one entry in every ``by_tag`` index the
    runner builds, so exactly one of them ever runs, the other vanishes without a word, and the
    summary still counts both as passed.  A run that reports "2 passed" having executed one
    command is not a partial failure; it is a false report.

    MISSING DEPENDENCY names a predecessor no step declares.  Left to the scheduler this is a
    "terminal starve" discovered only after every unrelated step has already run, so a typo in
    one edge costs a full build before it is reported.

    CYCLE is the crash.  Nothing downstream of the loader tolerates one: the bottom-level walk
    recurses along the cycle until the stack is exhausted, and the critical-path walk never
    reaches a sink.  Those walks are written for an acyclic graph on purpose; this is the check
    that makes that assumption true.  The refusal NAMES the cycle, because "there is a cycle" in
    a 200-node graph is not actionable.

    UNSATISFIABLE RESOURCE DEMAND is a step whose demand exceeds a POSITIVE declared cap, so it
    can never be admitted however long the run waits.  A cap declared as exactly ``0`` is
    deliberately NOT included: ``0`` means "blocked on purpose" (see
    :func:`undeclared_resource_demands`), and a check here would turn that documented affordance
    into a load error.

    Returns human-readable entries in a deterministic order, empty when the graph is sound.
    Duplicate tags SHORT-CIRCUIT the remaining checks: while two steps share a tag, every
    statement about "the step named X" is ambiguous, and reporting edges against an arbitrary
    winner would be guesswork presented as fact.
    """
    # Both language editions of this runner must refuse the same graphs with the same bytes, so
    # the message text and the traversal order here are a shared contract, pinned by the
    # cross-language differential.

    counts: dict[str, int] = {}
    for step in cfg.steps:
        counts[step.tag] = counts.get(step.tag, 0) + 1
    duplicates = sorted(tag for tag, n in counts.items() if n > 1)
    if duplicates:
        return [
            f"duplicate step tag '{tag}': declared {counts[tag]} times, but a tag names exactly "
            "one step -- only ONE of them would ever run and the rest would vanish silently"
            for tag in duplicates
        ]

    bad: list[str] = []
    known = set(counts)
    for step in cfg.steps:
        for dep in sorted(set(step.deps) - known):
            bad.append(
                f"step {step.tag}: depends on '{dep}', which no step declares"
            )

    # Iterative three-colour DFS over the dependency relation, matching
    # :func:`_refuse_unusable_explains`: iterative so a deep chain cannot hit the recursion limit
    # and turn a validation error into the very crash this check exists to prevent. Only edges
    # that RESOLVE are followed, so a missing dependency is reported once (above) rather than
    # also masquerading as a broken cycle.
    by_tag = {step.tag: step for step in cfg.steps}
    WHITE, GREY, BLACK = 0, 1, 2
    colour = {tag: WHITE for tag in by_tag}
    for root in sorted(by_tag):
        if colour[root] != WHITE:
            continue
        stack: list[tuple[str, bool]] = [(root, False)]
        path: list[str] = []
        while stack:
            tag, leaving = stack.pop()
            if leaving:
                colour[tag] = BLACK
                path.pop()
                continue
            if colour[tag] == BLACK:
                continue
            if colour[tag] == GREY:
                cycle = path[path.index(tag):] + [tag]
                bad.append("dependency cycle: " + " -> ".join(cycle))
                # ONE cycle per root, then abandon this root entirely: every node still on the
                # path is retired to BLACK so the outer loop cannot re-enter it and report the
                # same cycle again from a second entry point. A cycle elsewhere in the graph is
                # still reached from its own root.
                for pending in path:
                    colour[pending] = BLACK
                stack.clear()
                break
            colour[tag] = GREY
            path.append(tag)
            stack.append((tag, True))
            for dep in sorted(set(by_tag[tag].deps)):
                if dep in by_tag and colour.get(dep) != BLACK:
                    stack.append((dep, False))

    for step in cfg.steps:
        if step.skip_reason is not None:
            continue
        for name, count in sorted(step.hint.resources.items()):
            cap = cfg.resource_caps.get(name)
            if cap is not None and cap > 0 and count > cap:
                bad.append(
                    f"step {step.tag}: demands {name}={count} but resource_caps declares "
                    f"{name}={cap}, so it can never be admitted"
                )
    for step in cfg.steps:
        try:
            step.structured_test_results_manifest()
        except ValueError as error:
            bad.append(str(error))
    return bad


def write_domain_violations(cfg: DagConfig) -> list[str]:
    """Return fail-closed write-domain declaration errors in deterministic order.

    This predicate is called both while parsing and again at the scheduler entry
    point.  The second call covers library callers that construct ``DagConfig``
    directly, so a malformed in-memory graph cannot bypass the file parser.
    """

    policy = cfg.write_domain_policy
    active = policy.require_explicit or bool(policy.allowed_domains)
    if not active:
        return []

    bad: list[str] = []
    by_tag = cfg.by_tag()

    def has_immutable_barrier(step: Step) -> bool:
        pending = list(step.deps)
        seen: set[str] = set()
        while pending:
            tag = pending.pop()
            if tag in seen:
                continue
            seen.add(tag)
            ancestor = by_tag.get(tag)
            if ancestor is None:
                continue
            if (
                ancestor.write_domain_guarantee
                is WriteDomainGuarantee.IMMUTABLE_ARTIFACT_BARRIER
            ):
                return True
            pending.extend(ancestor.deps)
        return False

    for step in cfg.steps:
        domains = step.write_domains
        if domains is None:
            if policy.require_explicit:
                bad.append(f"{step.tag}: missing write_domains (use [] for no protected domains)")
            if step.write_domain_guarantee is not None:
                bad.append(f"{step.tag}: write_domain_guarantee requires write_domains")
            continue
        duplicates = sorted({name for name in domains if domains.count(name) > 1})
        if duplicates:
            bad.append(f"{step.tag}: duplicate write_domains: {', '.join(duplicates)}")
        unknown = sorted(set(domains) - policy.allowed_domains)
        if unknown:
            bad.append(f"{step.tag}: unknown write_domains: {', '.join(unknown)}")
        if domains and step.write_domain_guarantee is None:
            bad.append(f"{step.tag}: nonempty write_domains require write_domain_guarantee")
        if not domains and step.write_domain_guarantee is not None:
            bad.append(f"{step.tag}: write_domains=[] cannot claim a write guarantee")
        if (
            step.write_domain_guarantee
            is WriteDomainGuarantee.ARTIFACT_BARRIER_DEPENDENT
            and not has_immutable_barrier(step)
        ):
            bad.append(
                f"{step.tag}: artifact-barrier-dependent but no transitive dependency "
                "is an immutable-artifact-barrier"
            )
    return bad
