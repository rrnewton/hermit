"""Model, plan, visualize, and execute DAGs of CI/build steps.

Define your graph as :class:`Step` values (each carrying a :class:`ResourceHint`) in a
:class:`DagConfig`, then call :func:`run_dag_limited`. The runner gives you:

* independent active-step and total-CPU limits, with memory-aware step sizing,
* optional per-step containment and measurement through injected protocols,
* Graphviz + ASCII DAG visualization.

The command-line application establishes its Linux cgroup scope before invoking the scheduler.
Library calls to :func:`run_dag` are uncontained unless the caller supplies an enabled
:class:`CgroupManager`; passing no manager is an explicit process-group-only execution path.
Metrics are likewise opt-in through :class:`MetricsSink`.

    from dagrun import Step, ResourceHint, DagConfig, run_dag_limited, to_ascii
    cfg = DagConfig(steps=(Step("build", "app", "compile", "make build"),))
    print(to_ascii(cfg))
    result = run_dag_limited(cfg, max_steps=2, max_cpus=8)  # uncontained library execution
"""

from __future__ import annotations

from dagrun.ambient import (
    AmbientSnapshot,
    ambient_bucket,
    capture_ambient_snapshot,
)
from dagrun.attribution import require_step_end_ok
from dagrun.analyze import summarize
from dagrun.capabilities import (
    ENFORCEMENT_REGISTRY,
    Capability,
    Lane,
    enforcement_manifest,
    is_enforced,
)
from dagrun.cgroup import Cgroups, CgroupEnforcementKind, NoopCgroups
from dagrun.estimates import (
    DEFAULT_MIN_SAMPLES,
    Allocation,
    InfeasibleAllocationError,
    Plan,
    PlanEntry,
    Planner,
    SpeedupLevel,
    StepSamples,
    StepSpeedup,
    allocate_widths,
    apply_plan_to_config,
    build_plan,
    feedback_identity,
    load_step_samples,
    load_step_speedups,
    plan_to_json,
    plan_to_text,
    scaling_model_path,
    scaling_model_to_json,
    write_scaling_model,
)
from dagrun.io import (
    DagJsonError,
    MAX_DAG_FLATTENED_STEPS,
    MAX_DAG_INCLUDE_DEPTH,
    MAX_DAG_INCLUDE_INSTANCES,
    dag_from_json,
    dag_from_path,
    dag_from_yaml,
    dag_to_json,
    dag_to_yaml,
)
from dagrun.model import (
    DAG_CONFIG_FIELDS,
    DEFAULT_JOBS_FLAG,
    DAGRUN_EXTRA_ARGS_ENV,
    DEFAULT_STEP_TIMEOUT,
    WALL_CPU_BACKSTOP_FACTOR,
    DagManifest,
    DagConfig,
    CmdType,
    ResourceHint,
    ResultManifest,
    Step,
    StepClass,
    StructuredTestResultsManifest,
    STRUCTURED_TEST_RESULTS_KIND,
    STRUCTURED_TEST_RESULTS_PATH_ENV,
    STRUCTURED_TEST_RESULTS_CURRENT_SCHEMA,
    STRUCTURED_TEST_RESULTS_RETAINED_SCHEMA,
    WriteDomainGuarantee,
    WriteDomainPolicy,
    command_with_inner_jobs,
    command_uses_extra_args,
    cmdtype_env_with_inner_jobs,
    cmdtype_extra_args,
    dag_config_carry_diff,
    effective_jobs_flag,
    JOBS_ENV_ENV,
    step_width_is_resizable,
    resolve_jobs_env,
    effective_jobs_env,
    env_with_inner_jobs,
    validate_jobs_env_config,
    validate_cmdtype_config,
    preferred_inner_jobs,
    render_jobs_flag,
    result_manifest_owner,
    resolved_wall_timeout,
    step_classification,
    step_failure_reason,
    graph_structure_violations,
    undeclared_resource_demands,
    write_domain_violations,
)
from dagrun.perflog import CsvMetricsSink, PerfWindow
from dagrun.protocols import (
    CgroupManager,
    MetricsSink,
    RunResult,
    RunWindow,
    StepOutcome,
)
from dagrun.scheduler import (
    OUTER_RUN_ENV,
    Runner,
    cap_config_cpu_jobs,
    cap_config_max_cpus,
    nested_run_refusal,
    run_dag,
    run_dag_limited,
    steps_violating_run_timeout,
)
from dagrun.sizing import (
    jobs_footprint_bytes,
    jobs_for_budget,
    mem_available_bytes,
    parse_size,
    schedulable_peak_mem_bytes,
    step_mem_cap_bytes,
    step_mem_cap_for_inner_jobs,
    transitive_deps,
)
from dagrun.cgroup import (
    ScopeAttempt,
    ScopeAttemptKind,
    policy_skip_reason,
)
from dagrun.teardown import reap
from dagrun.viz import to_ascii, to_ascii_groups, to_dot, to_dot_groups

__version__: str = "0.15.0"

__all__ = [
    "__version__",
    # Enforcement manifest (derived per lane from the guards that implement it)
    "Capability",
    "ENFORCEMENT_REGISTRY",
    "Lane",
    "enforcement_manifest",
    "is_enforced",
    # DAG model
    "Step",
    "DagManifest",
    "ResultManifest",
    "StructuredTestResultsManifest",
    "STRUCTURED_TEST_RESULTS_KIND",
    "STRUCTURED_TEST_RESULTS_CURRENT_SCHEMA",
    "STRUCTURED_TEST_RESULTS_RETAINED_SCHEMA",
    "STRUCTURED_TEST_RESULTS_PATH_ENV",
    "CmdType",
    "StepClass",
    "ResourceHint",
    "DagConfig",
    "WriteDomainGuarantee",
    "WriteDomainPolicy",
    "DEFAULT_STEP_TIMEOUT",
    "WALL_CPU_BACKSTOP_FACTOR",
    "resolved_wall_timeout",
    "DEFAULT_JOBS_FLAG",
    "DAGRUN_EXTRA_ARGS_ENV",
    "step_classification",
    "preferred_inner_jobs",
    "step_failure_reason",
    "DAG_CONFIG_FIELDS",
    "dag_config_carry_diff",
    "graph_structure_violations",
    "undeclared_resource_demands",
    "write_domain_violations",
    "render_jobs_flag",
    "result_manifest_owner",
    "effective_jobs_flag",
    "JOBS_ENV_ENV",
    "step_width_is_resizable",
    "resolve_jobs_env",
    "effective_jobs_env",
    "env_with_inner_jobs",
    "validate_jobs_env_config",
    "validate_cmdtype_config",
    "command_with_inner_jobs",
    "command_uses_extra_args",
    "cmdtype_extra_args",
    "cmdtype_env_with_inner_jobs",
    # containment outcome
    "ScopeAttempt",
    "ScopeAttemptKind",
    "policy_skip_reason",
    # running
    "run_dag",
    "run_dag_limited",
    "cap_config_max_cpus",
    "cap_config_cpu_jobs",
    "nested_run_refusal",
    "OUTER_RUN_ENV",
    "steps_violating_run_timeout",
    "Runner",
    "RunResult",
    "StepOutcome",
    # pluggable protocols
    "CgroupManager",
    "MetricsSink",
    "RunWindow",
    # memory-aware sizing
    "schedulable_peak_mem_bytes",
    "jobs_footprint_bytes",
    "jobs_for_budget",
    "step_mem_cap_bytes",
    "step_mem_cap_for_inner_jobs",
    "transitive_deps",
    "parse_size",
    "mem_available_bytes",
    # containment
    "Cgroups",
    "NoopCgroups",
    "CgroupEnforcementKind",
    # metrics
    "CsvMetricsSink",
    "PerfWindow",
    # ambient load
    "AmbientSnapshot",
    "ambient_bucket",
    "capture_ambient_snapshot",
    # teardown
    "reap",
    # visualization
    "to_dot",
    "to_ascii",
    "to_dot_groups",
    "to_ascii_groups",
    # profile analysis library helper (not a console command)
    "summarize",
    # profile-store feedback + planner
    "Planner",
    "StepSamples",
    "SpeedupLevel",
    "StepSpeedup",
    "PlanEntry",
    "Allocation",
    "InfeasibleAllocationError",
    "Plan",
    "DEFAULT_MIN_SAMPLES",
    "feedback_identity",
    "load_step_samples",
    "load_step_speedups",
    "allocate_widths",
    "build_plan",
    "apply_plan_to_config",
    "plan_to_json",
    "plan_to_text",
    "scaling_model_path",
    "scaling_model_to_json",
    "write_scaling_model",
    # serialization
    "MAX_DAG_FLATTENED_STEPS",
    "MAX_DAG_INCLUDE_DEPTH",
    "MAX_DAG_INCLUDE_INSTANCES",
    "dag_from_json",
    "dag_from_path",
    "dag_from_yaml",
    "dag_to_json",
    "dag_to_yaml",
    "DagJsonError",
]
