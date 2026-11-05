//! Core DAG configuration, step, resource, and result types.

// Core DAG vocabulary for dagrun.
//
// Pure data + pure helpers, no I/O. A caller describes their build/test graph as a set of
// [`Step`] values (each carrying a [`ResourceHint`]) bundled in a [`DagConfig`], then hands
// it to the runner. Direct port of `py/dagrun/model.py`; the enum serde values, defaults,
// and the [`step_failure_reason`] precedence + strings are kept identical to the Python
// build so the two are cross-differential-testable.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;

use crate::test_results::{TestResult, TestResultsErrorKind};

/// Wall-clock backstop (seconds) for a step that declares NO wall budget AND no CPU budget to
/// derive one from. Wall time is LOAD-DEPENDENT, so it is only a defence-in-depth hang backstop;
/// the CPU-time budget is the real, load-immune guard.
pub const DEFAULT_STEP_TIMEOUT: i64 = 1800;

/// When a step declares a CPU-second budget but no wall budget, the wall backstop is derived at
/// this multiple of the (platform-scaled) CPU budget. A step legitimately spending C CPU-seconds
/// can take up to ~C wall-seconds when serialized on one core, plus scheduling slack under load;
/// 3x leaves generous headroom so the wall guard only ever fires on a true hang and never races
/// the authoritative CPU-second guard.
///
/// That reasoning holds for CPU-BOUND work only, which is why [`resolved_wall_timeout`] floors the
/// derived value at [`DEFAULT_STEP_TIMEOUT`] and never uses it to tighten a step: a step that
/// blocks on the network or a lock spends almost no CPU and arbitrary wall time.
///
/// The value and the name are DELIBERATELY the same as
/// `parallel_experiment_runner.model.WALL_CPU_BACKSTOP_FACTOR`, which established this idiom in
/// this repository. One policy, spelled once per project rather than invented twice.
pub const WALL_CPU_BACKSTOP_FACTOR: i64 = 3;

/// Default command-line template for a step's inner-job width.
pub const DEFAULT_JOBS_FLAG: &str = "-j";

/// Machine-level name of the environment variable through which the runner delivers a step's
/// admitted inner width. Cargo's `CARGO_BUILD_JOBS` is the motivating channel.
pub const JOBS_ENV_ENV: &str = "DAGRUN_JOBS_ENV";

/// Shell variable through which a known [`CmdType`] exposes its width arguments in a compound
/// command. It is deliberately absent for [`CmdType::Unknown`].
pub const DAGRUN_EXTRA_ARGS_ENV: &str = "DAGRUN_EXTRA_ARGS";

// Deliberately SMALL default caps for a step that DECLARES NOTHING — the "forcing function".
// An undeclared step is boxed into a tight 1-core / 1-GiB / 10-s-CPU floor, so a real step
// immediately hits the cap and must DECLARE its true needs, generating per-node resource
// metadata EMPIRICALLY (from measured breaches) instead of by guessing. Each applies ONLY
// when the step leaves the matching hint unset; an explicit hint wins. Mirror Python's
// `DEFAULT_SMALL_*` constants.
/// Default one-gibibyte memory cap applied to a step with no declared memory need.
pub const DEFAULT_SMALL_MEM_CAP_BYTES: i64 = 1024i64.pow(3); // 1 GiB inner memory.max
/// Default one-core limit applied to a step with no declared width.
pub const DEFAULT_SMALL_CPU_COUNT: i64 = 1; // 1-core cpu.max
/// Default CPU-time limit applied to a step with no declared budget, in seconds.
pub const DEFAULT_SMALL_CPU_TIMEOUT: i64 = 10; // 10 s CPU-time budget

/// Per-platform CPU-budget multiplier, applied at EXECUTION time to whatever CPU budget is in
/// effect for a step. A CPU second is load-immune (wall = cpu_busy + wait; contention inflates
/// only wait) but it is NOT clock-immune: a slower core retires the same instruction stream over
/// more seconds of CPU occupancy, so identical work legitimately burns more CPU-seconds on an
/// underpowered runner. A graph therefore carries ONE canonical `cpu_timeout` per step and the
/// platform scales it here.
///
/// Applying it at execution — rather than baking a second column of pre-multiplied numbers into
/// the graph — is the whole point: two independently-maintained timeout tables drift, and a step
/// has only one `cpu_timeout` field, so a per-platform column would force declaration authors to
/// pick a single number that is either too tight for the slow platform or too loose for the fast
/// one (hiding the very hangs the budget exists to catch).
///
/// 1.0 is a strict no-op and the default for every execution platform.
pub const DEFAULT_CPU_TIMEOUT_MULTIPLIER: f64 = 1.0;

/// Environment override for [`DEFAULT_CPU_TIMEOUT_MULTIPLIER`], so a CI lane can set the policy
/// once for its whole platform.
pub const CPU_TIMEOUT_MULTIPLIER_ENV: &str = "DAGRUN_CPU_TIMEOUT_MULTIPLIER";

/// Companion label naming the platform the multiplier describes; appears verbatim in the breach
/// message.
pub const CPU_TIMEOUT_PLATFORM_ENV: &str = "DAGRUN_CPU_TIMEOUT_PLATFORM";

/// How a step uses the machine, used for scheduling decisions.
///
/// The serde/string values (`"cpu-bound"`, `"latency-bound"`, `"light"`) are load-bearing:
/// they appear verbatim in JSON, `list`, `ascii`, and `dot` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepClass {
    /// Throughput scales primarily with CPU allocation.
    CpuBound,
    /// Completion is latency-sensitive rather than throughput-oriented.
    LatencyBound,
    /// The step has no special CPU or latency classification.
    #[default]
    Light,
}

/// Known command-line shapes for runner-controlled inner parallelism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CmdType {
    /// No known command-line shape; retain the existing jobs-flag and jobs-env behavior.
    #[default]
    Unknown,
    /// Make-compatible `-jN`.
    Make,
    /// `cargo build --jobs N`.
    CargoBuild,
    /// `cargo test --jobs N`.
    CargoTest,
    /// `cargo nextest run --test-threads N`.
    CargoNextest,
    /// A command accepting the common `-jN` spelling.
    GenericDashJCommand,
    /// A command whose `jobs_flag` explicitly describes the spelling.
    GenericWithFlag,
}

impl CmdType {
    /// Stable DAG-document value.
    pub fn value(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Make => "make",
            Self::CargoBuild => "cargo-build",
            Self::CargoTest => "cargo-test",
            Self::CargoNextest => "cargo-nextest",
            Self::GenericDashJCommand => "generic-dash-j-command",
            Self::GenericWithFlag => "generic-with-flag",
        }
    }

    /// Parse one stable DAG-document value.
    pub fn from_value(text: &str) -> Option<Self> {
        match text {
            "unknown" => Some(Self::Unknown),
            "make" => Some(Self::Make),
            "cargo-build" => Some(Self::CargoBuild),
            "cargo-test" => Some(Self::CargoTest),
            "cargo-nextest" => Some(Self::CargoNextest),
            "generic-dash-j-command" => Some(Self::GenericDashJCommand),
            "generic-with-flag" => Some(Self::GenericWithFlag),
            _ => None,
        }
    }
}

/// Closed vocabulary for nodes deliberately omitted before process spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentionalSkipReason {
    /// The manifest planner selected no test cells for this static bucket.
    EmptyManifestBucket,
}

impl IntentionalSkipReason {
    /// Return the stable serialized reason.
    pub fn value(self) -> &'static str {
        match self {
            Self::EmptyManifestBucket => "empty-manifest-bucket",
        }
    }

    /// Parse one stable serialized reason.
    pub fn from_value(text: &str) -> Option<Self> {
        match text {
            "empty-manifest-bucket" => Some(Self::EmptyManifestBucket),
            _ => None,
        }
    }
}

impl StepClass {
    /// Return the canonical serialized value.
    pub fn value(self) -> &'static str {
        match self {
            StepClass::CpuBound => "cpu-bound",
            StepClass::LatencyBound => "latency-bound",
            StepClass::Light => "light",
        }
    }

    /// Parse the canonical string form, or `None` for an unknown value.
    pub fn from_value(text: &str) -> Option<StepClass> {
        match text {
            "cpu-bound" => Some(StepClass::CpuBound),
            "latency-bound" => Some(StepClass::LatencyBound),
            "light" => Some(StepClass::Light),
            _ => None,
        }
    }
}

/// Why concurrent writes declared by a step are safe.
///
/// These values are deliberately not scheduler resources: artifact-shielded and
/// path-isolated writers retain parallelism instead of collapsing onto one global mutex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteDomainGuarantee {
    /// Consumers are shielded behind an immutable artifact barrier before later writers run.
    ArtifactBarrierDependent,
    /// The node atomically publishes the immutable artifact consumed by shielded nodes.
    ImmutableArtifactBarrier,
    /// The writer uses package/path-disjoint output.
    ExplicitlyIsolated,
    /// The node creates the mutable artifact set consumed by a later barrier.
    ArtifactProducer,
}

impl WriteDomainGuarantee {
    /// Canonical serialized spelling.
    pub fn value(self) -> &'static str {
        match self {
            Self::ArtifactBarrierDependent => "artifact-barrier-dependent",
            Self::ImmutableArtifactBarrier => "immutable-artifact-barrier",
            Self::ExplicitlyIsolated => "explicitly-isolated",
            Self::ArtifactProducer => "artifact-producer",
        }
    }

    /// Parse a canonical spelling.
    pub fn from_value(text: &str) -> Option<Self> {
        match text {
            "artifact-barrier-dependent" => Some(Self::ArtifactBarrierDependent),
            "immutable-artifact-barrier" => Some(Self::ImmutableArtifactBarrier),
            "explicitly-isolated" => Some(Self::ExplicitlyIsolated),
            "artifact-producer" => Some(Self::ArtifactProducer),
            _ => None,
        }
    }
}

/// Closed write-domain vocabulary and omission policy for one DAG.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WriteDomainPolicy {
    /// Require every step to carry `write_domains`, using `[]` for no protected artifact domains.
    pub require_explicit: bool,
    /// Domain names accepted by this DAG.
    pub allowed_domains: BTreeSet<String>,
}

/// Per-step scheduling knowledge: scarce-resource demand, cost estimate, memory.
///
/// Every field is optional (has a default). With none supplied the runner falls back to a
/// fixed concurrency with no memory model; supplying them enables memory-aware active-step sizing and
/// longest-processing-time dispatch ordering.
#[derive(Debug, Clone, Default)]
pub struct ResourceHint {
    /// Scarce-resource DEMAND for this step, e.g. `{"browser": 1}`. The runner never lets the
    /// summed demand of concurrently-running steps exceed `DagConfig::resource_caps`.
    pub resources: BTreeMap<String, i64>,
    /// Estimated wall-clock seconds; used only to order ready steps (longest first).
    pub est_duration_s: f64,
    /// Estimated peak resident memory (bytes). `None` excludes the step from the memory model.
    pub rss_baseline_bytes: Option<i64>,
    /// Width at which `rss_baseline_bytes` was measured exactly. Planner-derived transient
    /// provenance only: DAG readers/writers deliberately do not expose it. When present, sizing
    /// must not apply the fallback width heuristic a second time at the same width.
    pub rss_baseline_inner_jobs: Option<i64>,
    /// Explicit hard per-step memory cap (bytes); overrides the derived cap when set.
    pub hard_mem_max_bytes: Option<i64>,
    /// Scheduling classification for the step.
    pub classification: StepClass,
    /// Internal parallelism width for the step's own command (e.g. a build's `-j`).
    pub preferred_inner_jobs: Option<i64>,
    /// Effective CPU concurrency measured for the preferred width.
    pub measured_effective_cores: Option<f64>,
    /// Measured CPU utilization as a fraction of one allocated-width interval.
    pub measured_cpu_utilization: Option<f64>,
}

/// The manifest cell population selected by one DAG step.
///
/// This is serialized as the existing `manifest` object in DAG JSON. Keeping it on [`Step`]
/// means a consumer can use the declared lane and category directly instead of reconstructing
/// them from the shell command that happens to execute the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagManifest {
    /// Manifest lane selected by the step.
    pub lane: String,
    /// Manifest category selected by the step.
    pub category: String,
    /// Optional exact test selector within the lane and category.
    pub test: Option<String>,
    /// Optional exact execution-mode selector within the lane and category.
    pub mode: Option<String>,
    /// Optional exact backend selector within the lane and category.
    pub backend: Option<String>,
}

impl DagManifest {
    fn matches_exact_result(&self, result: &Self) -> bool {
        self.lane == result.lane
            && self.category == result.category
            && self
                .test
                .as_ref()
                .is_none_or(|value| result.test.as_ref() == Some(value))
            && self
                .mode
                .as_ref()
                .is_none_or(|value| result.mode.as_ref() == Some(value))
            && self
                .backend
                .as_ref()
                .is_none_or(|value| result.backend.as_ref() == Some(value))
    }

    fn exact_identity(&self) -> Result<String, String> {
        let mut missing = Vec::new();
        if self.lane.is_empty() {
            missing.push("lane");
        }
        if self.category.is_empty() {
            missing.push("category");
        }
        if self.test.as_deref().is_none_or(str::is_empty) {
            missing.push("test");
        }
        if self.mode.as_deref().is_none_or(str::is_empty) {
            missing.push("mode");
        }
        if self.backend.as_deref().is_none_or(str::is_empty) {
            missing.push("backend");
        }
        if !missing.is_empty() {
            return Err(format!(
                "result manifest is not an exact identity: missing {}",
                missing.join(", ")
            ));
        }
        Ok(format!(
            "{}/{}/{}/{}/{}",
            self.lane,
            self.category,
            self.test.as_deref().expect("checked above"),
            self.mode.as_deref().expect("checked above"),
            self.backend.as_deref().expect("checked above")
        ))
    }
}

/// Fixed wire identity for the per-step structured test-result artifact.
pub const STRUCTURED_TEST_RESULTS_KIND: &str = "structured-test-results";

/// A typed declaration that one step owns the scheduler-provided structured test-result path.
///
/// Kind and path are fixed by the variant. Schema is explicit because a graph migration must bind
/// the scheduler to the exact wire shape its producer writes. Owner binds the declaration to one
/// exact step. Both fields are checked structurally before run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredTestResultsManifest {
    /// Exact supported wire schema for the selected release line.
    pub schema: u64,
    /// Exact `group.job` tag of the step that owns this result.
    pub owner: String,
}

impl StructuredTestResultsManifest {
    /// Explicitly require complete classified per-attempt results (schema 3).
    pub fn classified(owner: impl Into<String>) -> Self {
        Self {
            schema: crate::test_results::CLASSIFIED_RESULTS_SCHEMA,
            owner: owner.into(),
        }
    }

    /// Require classified per-attempt results that may carry non-blocking diagnostic failures
    /// (schema 4).
    pub fn diagnostic(owner: impl Into<String>) -> Self {
        Self {
            schema: crate::test_results::DIAGNOSTIC_RESULTS_SCHEMA,
            owner: owner.into(),
        }
    }

    /// Declare current structured test results owned by one exact step tag.
    pub fn current(owner: impl Into<String>) -> Self {
        Self {
            schema: crate::test_results::CURRENT_SCHEMA,
            owner: owner.into(),
        }
    }

    /// Temporarily declare retained schema-2 results while a producer migration is atomic.
    pub fn retained_schema2(owner: impl Into<String>) -> Self {
        Self {
            schema: crate::test_results::RETAINED_RESULTS_SCHEMA,
            owner: owner.into(),
        }
    }
}

/// One current-result declaration.
///
/// Existing manifest-cell selectors retain their established JSON shape. Structured test
/// results use a tagged shape because a Cargo/Nextest result is not a manifest cell and must not
/// be represented by invented lane/category values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultManifest {
    /// Existing manifest-cell result selector.
    ManifestCell(DagManifest),
    /// Per-step structured test results at the scheduler path channel.
    StructuredTestResults(StructuredTestResultsManifest),
}

impl From<DagManifest> for ResultManifest {
    fn from(value: DagManifest) -> Self {
        Self::ManifestCell(value)
    }
}

/// One node in the DAG: a shell command plus its dependencies and resource hint.
#[derive(Debug, Clone)]
pub struct Step {
    /// Namespace component of the unique step tag.
    pub group: String,
    /// Job component of the unique step tag.
    pub job: String,
    /// Short human-readable label shown in command output.
    pub desc: String,
    /// Optional long-form documentation for this node (default empty). Unlike `desc` — a short
    /// label shown by `list`/`run` — `description` is free-form prose (often multi-line, e.g. a
    /// YAML block scalar) documenting WHY the step exists. It never affects scheduling.
    pub description: String,
    /// Named subsets this step belongs to, independent of its unique `group.job` tag.
    pub labels: Vec<String>,
    /// Shell command (`bash -c`), run from the run's working directory.
    pub cmd: String,
    /// Known command-line shape for delivering the admitted width.
    pub cmdtype: CmdType,
    /// Typed manifest selection carried by a manifest bucket step.
    pub manifest: Option<DagManifest>,
    /// Exact Cargo integration-test binary targets executed by this step.
    ///
    /// `None` means the serialized step did not declare this field. A present list is authoritative
    /// only when the consuming audit also verifies that the command executes the same targets.
    pub integration_test_binaries: Option<Vec<String>>,
    /// Typed result declarations owned by this step.
    ///
    /// `None` preserves the singular [`Step::manifest`] cell-selector declaration.
    /// `Some(vec![])` is authoritative and explicitly declares that the step produces no current
    /// results. Structured test evidence is a distinct variant, not a fabricated manifest cell.
    pub result_manifests: Option<Vec<ResultManifest>>,
    /// Tags (`"group.job"`) this step depends on.
    pub deps: Vec<String>,
    /// Environment variables added to the step process.
    pub env: BTreeMap<String, String>,
    /// Resource and scheduling metadata for this step.
    pub hint: ResourceHint,
    /// Skipped when networking is disabled.
    pub networkonly: bool,
    /// Selected only by an engine-only subset preset.
    pub engine_only: bool,
    /// Allow this step to launch nested dagrun schedulers inside a cgroup subtree delegated by
    /// the outer scheduler. False preserves the default nested-invocation refusal.
    pub delegated_children: bool,
    /// Wall-clock ceiling in seconds. `0` means UNDECLARED, not unlimited: the effective bound
    /// is then derived (see [`resolved_wall_timeout`]) from the step's CPU budget, or falls back
    /// to [`DEFAULT_STEP_TIMEOUT`]. A hardcoded 1800 here was the load-sensitive number baked
    /// into every graph that the derivation exists to remove.
    pub timeout: i64,
    // CPU-time budget in seconds (user+system, from the step's cgroup `cpu.stat`). `0` disables
    // the CPU-time guard, leaving only the wall `timeout`. CPU time is immune to machine load, so
    // this can be set far tighter than a load-tolerant wall timeout without flaking. Mirrors
    // Python's `Step.cpu_timeout`; both runners enforce it identically under cgroup boxing.
    /// User-plus-system CPU-time timeout in seconds; zero disables the guard.
    pub cpu_timeout: i64,
    /// Template for the inner-parallelism flag appended to `cmd` when this step declares
    /// `preferred_inner_jobs`. `None` inherits `DagConfig::default_jobs_flag`; an empty string
    /// disables appending (the step manages its own concurrency), making that declared width rigid
    /// rather than planner-adjustable. See [`render_jobs_flag`].
    pub jobs_flag: Option<String>,
    /// Environment variable through which this step accepts its worker count. `None` inherits
    /// [`DagConfig::default_jobs_env`]; an empty string disables the env channel for this step.
    pub jobs_env: Option<String>,
    /// Typed pre-execution omission, separate from PASS and dependency skip.
    pub skip_reason: Option<IntentionalSkipReason>,
    /// Presence-sensitive declaration: `None` is omitted; `Some(vec![])` explicitly declares no
    /// writes to the policy's protected artifact domains.
    pub write_domains: Option<Vec<String>>,
    /// Structural guarantee used for non-empty write-domain declarations.
    pub write_domain_guarantee: Option<WriteDomainGuarantee>,
    /// Tags (`"group.job"`) whose FAILURE this step exists to explain.
    ///
    /// A diagnostic node -- one whose only job is to name the cause of another node's failure --
    /// is by construction scheduled alongside something that fails, so eager-exit cancels it
    /// precisely when it was about to be useful. Observed in a consuming graph: a test node
    /// failed seconds before its companion ABI-comparison node, the companion was never launched,
    /// and the run reported the opaque symptom while the node that would have named the missing
    /// symbol produced nothing.
    ///
    /// Declaring the RELATIONSHIP rather than a "never cancel me" boolean keeps the intent
    /// visible, makes it checkable (the loader refuses an unknown tag, a self-reference and a
    /// cycle), and lets the exemption be CONDITIONAL -- see [`Step::explains_a_failure_in`].
    /// Both editions of this crate must accept and enforce this field identically.
    pub explains: Vec<String>,
    /// Non-empty name of the fail-fast family this step belongs to.
    ///
    /// A failure cancels running and queued peers in the same family. True dependents are still
    /// excluded by dependency closure, while independent families continue. `None` preserves the
    /// existing global eager-exit behavior so an existing graph cannot silently become a
    /// keep-going run merely because the runner learned this field.
    pub fail_fast_family: Option<String>,
}

impl Step {
    /// The step's unique tag, `"group.job"`.
    pub fn tag(&self) -> String {
        format!("{}.{}", self.group, self.job)
    }

    /// The manifest-cell selectors declared by this step, preserving the singular fallback.
    pub fn effective_result_manifests(&self) -> Cow<'_, [DagManifest]> {
        match self.result_manifests.as_deref() {
            Some(manifests) => Cow::Owned(
                manifests
                    .iter()
                    .filter_map(|manifest| match manifest {
                        ResultManifest::ManifestCell(manifest) => Some(manifest.clone()),
                        ResultManifest::StructuredTestResults(_) => None,
                    })
                    .collect(),
            ),
            None => Cow::Borrowed(self.manifest.as_slice()),
        }
    }

    /// The structured test-result declaration, when this step has one.
    ///
    /// Multiple declarations and an owner that differs from the containing step are refused here
    /// as well as by graph validation, so a direct caller cannot silently select the first one.
    pub fn structured_test_results_manifest(
        &self,
    ) -> Result<Option<&StructuredTestResultsManifest>, String> {
        let mut structured = self
            .result_manifests
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|manifest| match manifest {
                ResultManifest::StructuredTestResults(manifest) => Some(manifest),
                ResultManifest::ManifestCell(_) => None,
            });
        let first = structured.next();
        if structured.next().is_some() {
            return Err(format!(
                "step {}: declares structured test results more than once",
                self.tag()
            ));
        }
        if let Some(manifest) = first {
            if manifest.schema != crate::test_results::RETAINED_RESULTS_SCHEMA
                && manifest.schema != crate::test_results::CURRENT_SCHEMA
                && manifest.schema != crate::test_results::CLASSIFIED_RESULTS_SCHEMA
                && manifest.schema != crate::test_results::DIAGNOSTIC_RESULTS_SCHEMA
            {
                return Err(format!(
                    "step {}: structured test-result schema {} is unsupported; expected default schema {}, classified schema {} or diagnostic schema {}",
                    self.tag(), manifest.schema, crate::test_results::CURRENT_SCHEMA, crate::test_results::CLASSIFIED_RESULTS_SCHEMA, crate::test_results::DIAGNOSTIC_RESULTS_SCHEMA
                ));
            }
            if manifest.owner != self.tag() {
                return Err(format!(
                    "step {}: structured test-result owner '{}' must equal the containing step tag",
                    self.tag(),
                    manifest.owner
                ));
            }
        }
        Ok(first)
    }

    /// Whether this step is exempt from eager-exit given the set of tags that genuinely FAILED.
    ///
    /// Deliberately narrow: declaring `explains` does not make a step immortal, it only protects
    /// the step when one of the specific nodes it claims to explain has actually failed. A
    /// diagnostic that explains nothing about THIS failure is reaped like any other peer, so
    /// eager-exit keeps doing its job everywhere else.
    pub fn explains_a_failure_in(&self, failed: &HashSet<String>) -> bool {
        self.explains.iter().any(|tag| failed.contains(tag))
    }
}

/// Resolve one exact manifest result to the unique step that declares ownership of it.
///
/// The caller supplies the selected steps, so this helper stays independent of any client's
/// manifest catalogue and label vocabulary. An exact result must name every identity dimension;
/// zero matching steps and multiple matching steps are both refusals.
pub fn result_manifest_owner<'a>(
    steps: &'a [Step],
    result: &DagManifest,
) -> Result<&'a Step, String> {
    let identity = result.exact_identity()?;
    let owners: Vec<&Step> = steps
        .iter()
        .filter(|step| {
            step.effective_result_manifests()
                .iter()
                .any(|selector| selector.matches_exact_result(result))
        })
        .collect();
    match owners.as_slice() {
        [] => Err(format!(
            "result manifest {identity} has no owning step in the selected DAG"
        )),
        [owner] => Ok(*owner),
        _ => Err(format!(
            "result manifest {identity} has multiple owning steps in the selected DAG: {}",
            owners
                .iter()
                .map(|step| step.tag())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Render an inner-parallelism flag from a template and worker count.
///
/// A `%d` placeholder is substituted directly, a template ending in `=` is concatenated directly,
/// and every other template is separated from the count by one space.
pub fn render_jobs_flag(template: &str, inner_jobs: i64) -> String {
    if template.contains("%d") {
        return template.replace("%d", &inner_jobs.to_string());
    }
    if template.ends_with('=') {
        return format!("{template}{inner_jobs}");
    }
    format!("{template} {inner_jobs}")
}

/// The jobs-flag template in effect for a step: its own `jobs_flag` overrides the
/// DagConfig-level default; `None` inherits the default.
pub fn effective_jobs_flag<'a>(step: &'a Step, default_jobs_flag: &'a str) -> &'a str {
    step.jobs_flag.as_deref().unwrap_or(default_jobs_flag)
}

fn normalize_jobs_env(raw: &str, source: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Ok(String::new());
    }
    let mut bytes = name.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic());
    if !valid_first || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric()) {
        return Err(format!(
            "{source}={name:?} is not a valid environment variable name"
        ));
    }
    if matches!(
        name,
        "DAGRUN_DELEGATED_CGROUP"
            | "DAGRUN_DELEGATED_UNBOXED"
            | "DAGRUN_EXTRA_ARGS"
            | "DAGRUN_OUTER_RUN"
            | "DAGRUN_STEP"
    ) {
        return Err(format!(
            "{source}={name:?} is reserved by dagrun and cannot carry a step's worker count"
        ));
    }
    // Assignment alone cannot prove these shell-control names are ordinary width channels.
    if matches!(
        name,
        "BASH_COMPAT"
            | "BASH_ENV"
            | "BASH_XTRACEFD"
            | "CDPATH"
            | "ENV"
            | "EXECIGNORE"
            | "GLOBIGNORE"
            | "PATH"
            | "POSIXLY_CORRECT"
    ) {
        return Err(format!("{source}={name:?} is a shell startup/control variable, not a safe jobs environment channel"));
    }
    Ok(name.to_string())
}

/// Resolve the machine's default inner-width environment channel.
///
/// An explicit value wins over [`JOBS_ENV_ENV`]; absent input means no env channel. Malformed
/// names are refused because silently disabling the channel would make an env-only step appear
/// resizable while leaving its guest width uncontrolled.
pub fn resolve_jobs_env(explicit: Option<&str>) -> Result<String, String> {
    match explicit {
        Some(value) => normalize_jobs_env(value, JOBS_ENV_ENV),
        None => match std::env::var(JOBS_ENV_ENV) {
            Ok(value) => normalize_jobs_env(&value, JOBS_ENV_ENV),
            Err(std::env::VarError::NotPresent) => Ok(String::new()),
            Err(std::env::VarError::NotUnicode(_)) => Err(format!(
                "{JOBS_ENV_ENV} is not valid UTF-8 and cannot name an environment variable"
            )),
        },
    }
}

/// The inner-width environment channel in effect for a step.
pub fn effective_jobs_env<'a>(step: &'a Step, default_jobs_env: &'a str) -> &'a str {
    let (name, source) = match step.jobs_env.as_deref() {
        Some(name) => (name, "jobs_env"),
        None => (default_jobs_env, "default_jobs_env"),
    };
    normalize_jobs_env(name, source)
        .unwrap_or_else(|error| panic!("invalid jobs-env configuration: {error}"));
    name.trim()
}

/// Return the environment assignment carrying a step's admitted width, when configured.
pub fn env_with_inner_jobs(
    step: &Step,
    default_jobs_env: &str,
    inner_jobs: Option<i64>,
) -> Option<(String, String)> {
    let width = inner_jobs?;
    let name = effective_jobs_env(step, default_jobs_env).trim();
    (!name.is_empty()).then(|| (name.to_string(), width.to_string()))
}

/// Whether the runner can change a step's guest-visible inner width through cmdtype, argv, or
/// environment.
pub fn step_width_is_resizable(
    step: &Step,
    default_jobs_flag: &str,
    default_jobs_env: &str,
) -> bool {
    // Resolve the env channel first so a valid jobs flag cannot short-circuit validation of a
    // malformed programmatic jobs_env value.
    let jobs_env = effective_jobs_env(step, default_jobs_env);
    if step.cmdtype != CmdType::Unknown {
        return true;
    }
    !effective_jobs_flag(step, default_jobs_flag)
        .trim()
        .is_empty()
        || !jobs_env.trim().is_empty()
}

/// Validate all jobs-env channels in a programmatically constructed configuration.
pub fn validate_jobs_env_config(cfg: &DagConfig) -> Result<(), String> {
    normalize_jobs_env(&cfg.default_jobs_env, "default_jobs_env")?;
    for step in &cfg.steps {
        if let Some(value) = &step.jobs_env {
            normalize_jobs_env(value, &format!("{}.jobs_env", step.tag()))?;
        }
    }
    Ok(())
}

/// Refuse an invalid programmatic jobs-env configuration at APIs that cannot return a typed error.
pub(crate) fn assert_valid_jobs_env_config(cfg: &DagConfig) {
    if let Err(error) = validate_jobs_env_config(cfg) {
        panic!("invalid jobs-env configuration: {error}");
    }
}

/// Return a step command with its effective inner-job arguments when requested.
///
/// A known cmdtype supplies its own spelling and leaves a command containing
/// `DAGRUN_EXTRA_ARGS` unchanged. `unknown` retains the existing jobs-flag behavior.
pub fn command_with_inner_jobs(
    step: &Step,
    default_jobs_flag: &str,
    inner_jobs: Option<i64>,
) -> String {
    if step.cmdtype != CmdType::Unknown {
        let Some(extra_args) = cmdtype_extra_args(step, inner_jobs) else {
            return step.cmd.clone();
        };
        if command_uses_extra_args(&step.cmd) {
            return step.cmd.clone();
        }
        return format!("{} {extra_args}", step.cmd);
    }
    match inner_jobs {
        None => step.cmd.clone(),
        Some(n) => {
            let template = effective_jobs_flag(step, default_jobs_flag);
            if template.trim().is_empty() {
                step.cmd.clone()
            } else {
                format!("{} {}", step.cmd, render_jobs_flag(template, n))
            }
        }
    }
}

/// Whether a command contains either documented `DAGRUN_EXTRA_ARGS` expansion.
pub fn command_uses_extra_args(command: &str) -> bool {
    extra_args_references(command).0
}

fn extra_args_references(command: &str) -> (bool, bool, bool) {
    let bytes = command.as_bytes();
    let braced = b"${DAGRUN_EXTRA_ARGS}";
    let plain = b"$DAGRUN_EXTRA_ARGS";
    let mut present = false;
    let mut single_quoted = false;
    let mut double_quoted = false;
    let mut quote = 0u8;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if quote != b'\'' && byte == b'\\' {
            index = (index + 2).min(bytes.len());
            continue;
        }
        if byte == b'\'' || byte == b'"' {
            if quote == 0 {
                quote = byte;
            } else if quote == byte {
                quote = 0;
            }
            index += 1;
            continue;
        }
        let braced_match = bytes[index..].starts_with(braced);
        let plain_match = bytes[index..].starts_with(plain)
            && bytes
                .get(index + plain.len())
                .is_none_or(|next| *next != b'_' && !next.is_ascii_alphanumeric());
        let matched = if braced_match {
            braced.len()
        } else if plain_match {
            plain.len()
        } else {
            0
        };
        if matched > 0 {
            present = true;
            single_quoted |= quote == b'\'';
            double_quoted |= quote == b'"';
            index += matched;
        } else {
            index += 1;
        }
    }
    (present, single_quoted, double_quoted)
}

fn command_is_compound(command: &str) -> bool {
    let bytes = command.as_bytes();
    let mut quote = 0u8;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if quote != b'\'' && byte == b'\\' {
            index = (index + 2).min(bytes.len());
            continue;
        }
        if byte == b'\'' || byte == b'"' {
            if quote == 0 {
                quote = byte;
            } else if quote == byte {
                quote = 0;
            }
            index += 1;
            continue;
        }
        if quote == 0 && (matches!(byte, b';' | b'\n' | b'|') || bytes[index..].starts_with(b"&&"))
        {
            return true;
        }
        index += 1;
    }
    false
}

/// Width arguments supplied by a known command type.
pub fn cmdtype_extra_args(step: &Step, inner_jobs: Option<i64>) -> Option<String> {
    let width = inner_jobs?;
    match step.cmdtype {
        CmdType::Unknown => None,
        CmdType::Make | CmdType::GenericDashJCommand => Some(format!("-j{width}")),
        CmdType::CargoBuild | CmdType::CargoTest => Some(format!("--jobs {width}")),
        CmdType::CargoNextest => Some(format!("--test-threads {width}")),
        CmdType::GenericWithFlag => Some(render_jobs_flag(
            step.jobs_flag
                .as_deref()
                .expect("validated generic-with-flag requires jobs_flag"),
            width,
        )),
    }
}

/// `DAGRUN_EXTRA_ARGS` assignment for a known command type.
pub fn cmdtype_env_with_inner_jobs(
    step: &Step,
    inner_jobs: Option<i64>,
) -> Option<(String, String)> {
    cmdtype_extra_args(step, inner_jobs).map(|args| (DAGRUN_EXTRA_ARGS_ENV.to_string(), args))
}

/// Refuse ambiguous cmdtype configuration and wrong multi-token quoting.
pub fn validate_cmdtype_config(cfg: &DagConfig) -> Result<(), String> {
    for step in &cfg.steps {
        if step.cmdtype == CmdType::GenericWithFlag
            && step
                .jobs_flag
                .as_deref()
                .is_none_or(|flag| flag.trim().is_empty())
        {
            return Err(format!(
                "step {}: cmdtype generic-with-flag requires a non-empty jobs_flag",
                step.tag()
            ));
        }
        if !matches!(step.cmdtype, CmdType::Unknown | CmdType::GenericWithFlag)
            && step
                .jobs_flag
                .as_deref()
                .is_some_and(|flag| !flag.trim().is_empty())
        {
            return Err(format!(
                "step {}: jobs_flag is valid with cmdtype generic-with-flag, not {}",
                step.tag(),
                step.cmdtype.value()
            ));
        }
        if let Some(extra_args) = cmdtype_extra_args(step, Some(1)) {
            let (present, single_quoted, double_quoted) = extra_args_references(&step.cmd);
            if command_is_compound(&step.cmd) && !present {
                return Err(format!(
                    "step {}: compound cmd with cmdtype {} must place unquoted \
                     $DAGRUN_EXTRA_ARGS or ${{DAGRUN_EXTRA_ARGS}} where the width belongs",
                    step.tag(),
                    step.cmdtype.value()
                ));
            }
            if single_quoted {
                return Err(format!(
                    "step {}: DAGRUN_EXTRA_ARGS must not be single-quoted because the shell \
                     would not expand it",
                    step.tag()
                ));
            }
            if extra_args.split_whitespace().count() > 1 && double_quoted {
                return Err(format!(
                    "step {}: DAGRUN_EXTRA_ARGS must be unquoted for cmdtype {} because it \
                     contains multiple shell words; quoting it would pass one argument",
                    step.tag(),
                    step.cmdtype.value()
                ));
            }
        }
    }
    Ok(())
}

/// Refuse an invalid programmatic cmdtype configuration at APIs without a typed error.
pub(crate) fn assert_valid_cmdtype_config(cfg: &DagConfig) {
    if let Err(error) = validate_cmdtype_config(cfg) {
        panic!("invalid cmdtype configuration: {error}");
    }
}

/// Resolve a step's scheduling class from its explicit hint and resource demands.
pub fn step_classification(step: &Step) -> StepClass {
    if step.hint.classification != StepClass::Light {
        return step.hint.classification;
    }
    if step.hint.resources.contains_key("browser") {
        return StepClass::LatencyBound;
    }
    StepClass::Light
}

/// Positive internal parallelism width: an explicit override wins, else the hint.
///
/// Zero/negative library-authored values mean undeclared and fall through to the configured
/// per-step CPU default rather than becoming an invalid command flag or declared guest width.
pub fn preferred_inner_jobs(step: &Step, experiment_override: Option<i64>) -> Option<i64> {
    experiment_override
        .or(step.hint.preferred_inner_jobs)
        .filter(|value| *value > 0)
}

/// CANONICAL CPU-time budget (seconds) for a step, before any per-platform scaling: its declared
/// `cpu_timeout` (>0) wins; otherwise the DAG's SMALL default. Both 0 means the guard is
/// disabled. This is the number a graph declares — one table, platform-independent.
pub fn canonical_cpu_timeout(step: &Step, default_cpu_timeout: i64) -> i64 {
    if step.cpu_timeout > 0 {
        step.cpu_timeout
    } else {
        default_cpu_timeout
    }
}

/// Apply a per-platform multiplier to a canonical CPU budget.
///
/// Rounds to whole seconds (the enforcement poll is 1 Hz) and never rounds a live budget down to
/// 0 — that would silently DISABLE the guard on a platform with a small multiplier, turning a
/// scaling policy into an opt-out. A disabled budget (canonical 0) stays disabled.
pub fn scale_cpu_timeout(canonical: i64, multiplier: f64) -> i64 {
    if canonical <= 0 {
        return 0;
    }
    if multiplier == DEFAULT_CPU_TIMEOUT_MULTIPLIER {
        return canonical;
    }
    let scaled = (canonical as f64 * multiplier).round() as i64;
    scaled.max(1)
}

/// CPU-time budget actually ENFORCED for a step on this platform: the canonical budget scaled by
/// the platform multiplier. With the default 1.0 multiplier this is exactly the canonical budget.
pub fn effective_cpu_timeout(step: &Step, default_cpu_timeout: i64, multiplier: f64) -> i64 {
    scale_cpu_timeout(canonical_cpu_timeout(step, default_cpu_timeout), multiplier)
}

/// The wall-clock ceiling a step is actually run under, deriving one when none was declared.
///
/// Precedence, most specific first:
///
/// 1. the step's own `timeout` (>0) — an explicit author decision always wins;
/// 2. the document's `default_step_timeout` (>0) — an explicit document-wide decision;
/// 3. `WALL_CPU_BACKSTOP_FACTOR` x the step's PLATFORM-SCALED `cpu_timeout`, when the step
///    DECLARED one AND that is LARGER than [`DEFAULT_STEP_TIMEOUT`];
/// 4. [`DEFAULT_STEP_TIMEOUT`].
///
/// *THE DERIVATION ONLY EVER LOOSENS.* Rule 3 is floored at [`DEFAULT_STEP_TIMEOUT`], so no step
/// that ran under 1800 s before this rule existed runs under less now. Without that floor the rule
/// silently retimed every already-authored step that declared a CPU budget: a `networkonly` step
/// `{"cmd": "git fetch ...", "cpu_timeout": 5}` burns ~5 CPU-seconds and blocks for minutes on the
/// network, and a 15-second ceiling SIGTERMs it and reports a hang. Wall time is unbounded
/// relative to CPU time for anything that blocks, so a CPU-derived ceiling is only sound as an
/// UPPER bound. The direction the derivation is for is the other one: a step declaring
/// `cpu_timeout: 900` had a 1800 s wall ceiling that its own CPU guard could reach — at a 2.5x
/// platform multiplier the enforced budget is 2250 s, ABOVE the wall bound — so the wall guard
/// fired first and reported a hang where the truth was a slow machine. Rule 3 lifts that step to
/// 2700 s and restores the 3x margin.
///
/// Two further choices in rule 3 are deliberate and were the open questions in the design:
///
/// *DECLARED, not canonical.* [`canonical_cpu_timeout`] fills in the DAG's small default (10 s)
/// for a step that declares nothing, and it is ALWAYS in force. Deriving from that would hand
/// every undeclared step a 30-second wall ceiling where it currently gets 1800 — a silent,
/// enormous tightening applied to exactly the steps whose needs nobody has measured yet. So the
/// derivation fires only for a step whose author stated a CPU budget, and everything else falls
/// to rule 4 with the behaviour it has always had.
///
/// *SCALED, not canonical.* `cpu_timeout_multiplier` exists to loosen the CPU guard on a slow
/// platform. A wall backstop pinned to the unscaled number would shrink to 3/multiplier of the
/// enforced budget and start racing — firing FIRST on precisely the platform the multiplier was
/// added for, and reporting a wall hang where the truth is a slow machine. Tracking the scaled
/// budget keeps the 3x ratio wherever the multiplier goes.
pub fn resolved_wall_timeout(step: &Step, default_step_timeout: i64, multiplier: f64) -> i64 {
    if step.timeout > 0 {
        return step.timeout;
    }
    if default_step_timeout > 0 {
        return default_step_timeout;
    }
    if step.cpu_timeout > 0 {
        let derived = WALL_CPU_BACKSTOP_FACTOR * scale_cpu_timeout(step.cpu_timeout, multiplier);
        return derived.max(DEFAULT_STEP_TIMEOUT);
    }
    DEFAULT_STEP_TIMEOUT
}

/// Resolve the platform CPU-budget multiplier and its label from an explicit value then the
/// environment. A malformed or non-positive environment value is REFUSED rather than silently
/// ignored — a typo that quietly reverted to 1.0 would loosen enforcement invisibly.
pub fn resolve_cpu_timeout_multiplier(explicit: Option<f64>) -> Result<(f64, String), String> {
    let label = std::env::var(CPU_TIMEOUT_PLATFORM_ENV)
        .unwrap_or_default()
        .trim()
        .to_string();
    if let Some(value) = explicit {
        if value <= 0.0 {
            return Err(format!("cpu-timeout multiplier must be > 0, got {value}"));
        }
        return Ok((value, label));
    }
    let raw = std::env::var(CPU_TIMEOUT_MULTIPLIER_ENV).unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok((DEFAULT_CPU_TIMEOUT_MULTIPLIER, label));
    }
    let value: f64 = raw
        .parse()
        .map_err(|_| format!("{CPU_TIMEOUT_MULTIPLIER_ENV}={raw:?} is not a number"))?;
    if value <= 0.0 {
        return Err(format!("{CPU_TIMEOUT_MULTIPLIER_ENV}={raw:?} must be > 0"));
    }
    Ok((value, label))
}

/// `" (canonical 3s x2 github-hosted)"` when a platform multiplier scaled the budget, else empty.
/// Silent at 1.0 so the common unscaled message is unchanged.
fn cpu_timeout_policy_suffix(canonical: i64, multiplier: f64, platform: &str) -> String {
    if multiplier == DEFAULT_CPU_TIMEOUT_MULTIPLIER || canonical <= 0 {
        return String::new();
    }
    let rendered = format_multiplier(multiplier);
    let label = if platform.is_empty() {
        String::new()
    } else {
        format!(" {platform}")
    };
    format!(" (canonical {canonical}s x{rendered}{label})")
}

/// Render a multiplier compactly (`2.0 -> "2"`, `1.5 -> "1.5"`) for stable breach strings.
fn format_multiplier(value: f64) -> String {
    let mut s = format!("{value}");
    if let Some(stripped) = s.strip_suffix(".0") {
        s = stripped.to_string();
    }
    s
}

/// Core cap (cgroup `cpu.max`) in effect for a step: its declared `preferred_inner_jobs` wins;
/// otherwise the DAG's SMALL default. Bounds cpu.max ONLY, never the command's inner `-j` flag
/// (which stays keyed to the declared width).
pub fn effective_cpu_count(step: &Step, default_cpu_count: Option<i64>) -> Option<i64> {
    preferred_inner_jobs(step, None).or(default_cpu_count.filter(|value| *value > 0))
}

// Map a Unix signal number to its name (e.g. `9 -> "SIGKILL"`), matching the names Python's
// `signal.Signals(n).name` produces for the common signals; unknown numbers fall back to
// `"signal N"` exactly like the Python `ValueError` branch.
fn signal_name(sig: i64) -> String {
    let name = match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        11 => "SIGSEGV",
        12 => "SIGUSR2",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        17 => "SIGCHLD",
        18 => "SIGCONT",
        19 => "SIGSTOP",
        20 => "SIGTSTP",
        21 => "SIGTTIN",
        22 => "SIGTTOU",
        24 => "SIGXCPU",
        25 => "SIGXFSZ",
        26 => "SIGVTALRM",
        27 => "SIGPROF",
        28 => "SIGWINCH",
        29 => "SIGIO",
        31 => "SIGSYS",
        _ => return format!("signal {sig}"),
    };
    name.to_string()
}

/// Describe a failed step without conflating an external signal with an OOM.
///
/// Failure-reason precedence is:
/// OOM > CPU-timeout > timeout > pids-guard > detail-capture-failure > signal > exit code.
///
/// This function formats outer process and resource failures. The scheduler reports a
/// structured-result refusal or terminal test failure when no outer failure takes precedence;
/// a simultaneous required-result refusal is retained separately in `test_results_error`.
///
/// A negative `returncode` alone only says that the child received a Unix signal; it must not be
/// reported as an OOM without a positive kernel `memory.events` OOM counter. A delegated boundary
/// may report `oom` while its descendant victim owns `oom_kill`, so zero local kills do not erase
/// otherwise-conclusive OOM evidence.
#[allow(clippy::too_many_arguments)]
pub fn step_failure_reason(
    returncode: Option<i64>,
    oomed: bool,
    oom_kills: i64,
    timed_out: bool,
    timeout: i64,
    pids_guard_tripped: bool,
    pids_guard_reason: Option<&str>,
    detail_write_failure: &[String],
    cpu_timed_out: bool,
    cpu_timeout: i64,
    cpu_timeout_canonical: i64,
    cpu_timeout_multiplier: f64,
    cpu_timeout_platform: &str,
) -> String {
    if oomed {
        if oom_kills == 0 {
            return "MEMORY CAP EXHAUSTED (kernel reported OOM at inner MemoryMax; no local \
                    oom_kill counter)"
                .to_string();
        }
        return format!("OOM-KILLED (hit inner MemoryMax; {oom_kills} oom_kill event(s))");
    }
    if cpu_timed_out {
        // When a platform multiplier is in effect the enforced number is NOT the number written
        // in the graph, so the message must carry both plus the policy that connects them.
        let suffix = cpu_timeout_policy_suffix(
            cpu_timeout_canonical,
            cpu_timeout_multiplier,
            cpu_timeout_platform,
        );
        return format!("CPU-TIMEOUT >{cpu_timeout}s cpu{suffix}");
    }
    if timed_out {
        return format!("TIMEOUT >{timeout}s");
    }
    if pids_guard_tripped {
        return format!("PIDS GUARD ({})", pids_guard_reason.unwrap_or(""));
    }
    if let Some(first) = detail_write_failure.first() {
        return format!("DETAIL CAPTURE FAILED ({first})");
    }
    if let Some(rc) = returncode {
        if rc < 0 {
            let name = signal_name(-rc);
            return format!(
                "received {name} with no validate timeout, pids guard, \
                 or child-cgroup OOM recorded"
            );
        }
    }
    match returncode {
        Some(rc) => format!("exit {rc}"),
        None => "exit None".to_string(),
    }
}

/// A complete step graph and its scheduling, memory, and containment policy.
#[derive(Debug, Clone)]
pub struct DagConfig {
    /// Steps in deterministic declaration order.
    pub steps: Vec<Step>,
    // Optional long-form documentation for the WHOLE DAG (default empty). Free-form prose
    // describing the pipeline as a whole; never affects scheduling.
    /// Optional long-form description of the whole DAG.
    pub description: String,
    /// Maximum concurrent capacity for each named scarce resource.
    pub resource_caps: BTreeMap<String, i64>,
    /// Multiplier from a step's measured RSS baseline to its inner memory cap (headroom).
    pub mem_cap_factor: f64,
    /// Lower bound (bytes) on the modeled worst-case footprint. Default 8 GiB.
    pub mem_cap_floor_bytes: i64,
    /// Multiplier applied to the modeled peak to leave headroom. 1.0 = no inflation.
    pub outer_mem_safety_factor: f64,
    /// Document-wide wall budget for steps that omit their own. `0` means the document declared
    /// none, so each step derives its own (see [`resolved_wall_timeout`]).
    pub default_step_timeout: i64,
    /// Default inner-parallelism flag template for steps that don't set their own `jobs_flag`.
    pub default_jobs_flag: String,
    /// Default inner-parallelism environment channel for steps without their own `jobs_env`.
    /// Empty means this machine offers no environment channel.
    pub default_jobs_env: String,
    /// Deliberately SMALL default inner memory.max (bytes) for a step that declares NO memory
    /// hint (the forcing function; see `DEFAULT_SMALL_MEM_CAP_BYTES`). `None` disables it.
    pub default_step_mem_cap_bytes: Option<i64>,
    /// Deliberately SMALL default core cap (cgroup cpu.max) for a step that declares no inner
    /// width (see `DEFAULT_SMALL_CPU_COUNT`). `None` disables it. Bounds cpu.max only, never
    /// the command's `-j` flag.
    pub default_step_cpu_count: Option<i64>,
    /// Deliberately SMALL default CPU-time budget (seconds) for a step whose `cpu_timeout` is
    /// unset (see `DEFAULT_SMALL_CPU_TIMEOUT`). `0` disables it.
    pub default_step_cpu_timeout: i64,
    /// Execution-time multiplier over the canonical CPU budget for THIS platform (see
    /// `DEFAULT_CPU_TIMEOUT_MULTIPLIER`). Caller/platform policy, never persisted with the graph.
    pub cpu_timeout_multiplier: f64,
    /// Free-form platform label reported alongside the multiplier in a breach message.
    pub cpu_timeout_platform: String,
    /// Fail-closed write-domain policy. Default is disabled for generic DAGs that do not opt in.
    pub write_domain_policy: WriteDomainPolicy,
}

impl Default for DagConfig {
    fn default() -> Self {
        DagConfig {
            steps: Vec::new(),
            description: String::new(),
            resource_caps: BTreeMap::new(),
            mem_cap_factor: 1.25,
            mem_cap_floor_bytes: 8 * 1024i64.pow(3),
            outer_mem_safety_factor: 1.0,
            default_step_timeout: 0,
            default_jobs_flag: DEFAULT_JOBS_FLAG.to_string(),
            default_jobs_env: String::new(),
            // The SMALL forcing-function caps are active by default. The declarations-first
            // migration supplied measured budgets for nodes that exceed the floor; an explicit
            // per-step declaration still wins. `--unsafe-no-cgroups` is the deliberately loud
            // escape hatch for an unboxed run.
            default_step_mem_cap_bytes: Some(DEFAULT_SMALL_MEM_CAP_BYTES),
            default_step_cpu_count: Some(DEFAULT_SMALL_CPU_COUNT),
            default_step_cpu_timeout: DEFAULT_SMALL_CPU_TIMEOUT,
            cpu_timeout_multiplier: DEFAULT_CPU_TIMEOUT_MULTIPLIER,
            cpu_timeout_platform: String::new(),
            write_domain_policy: WriteDomainPolicy::default(),
        }
    }
}

impl DagConfig {
    /// Index the steps by their unique fully qualified tags.
    pub fn by_tag(&self) -> BTreeMap<String, &Step> {
        self.steps.iter().map(|s| (s.tag(), s)).collect()
    }

    /// This configuration's POLICY carried forward onto a different step list.
    ///
    /// The safe replacement for `DagConfig { steps, ..Default::default() }`, which is how the
    /// dropped-field bug is written every time: `..Default::default()` reverts every field the
    /// literal does not name, and the reverted fields appear in no diff, no warning and no
    /// failure. Take a lane's steps and call this on the config they came from, and the caps,
    /// timeouts and memory policy travel with them by construction.
    #[must_use]
    pub fn with_steps(&self, steps: Vec<Step>) -> DagConfig {
        let mut out = self.clone();
        out.steps = steps;
        out
    }
}

/// Every top-level `DagConfig` field, in declaration order — the checklist `carry_diff` walks.
///
/// Written down so a reader can see the whole surface at once; the compiler holds it honest,
/// because [`dag_config_carry_diff`] destructures `DagConfig` exhaustively (no `..`) and a new
/// field therefore fails to build until it is given a comparison here too.
pub const DAG_CONFIG_FIELDS: [&str; 15] = [
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
    "cpu_timeout_multiplier",
    "cpu_timeout_platform",
    "write_domain_policy",
];

/// Every top-level field whose value DIFFERS between two configurations, named with both values.
///
/// A CARRY ASSERTION. A consumer that loads a DAG file, keeps its steps and rebuilds the config
/// silently substitutes a default for every field it did not name — a 600 s wall budget becomes
/// 1800 s, an 8 GiB floor becomes whatever the constant says, and NOTHING reports it. A cap that
/// silently becomes a default is indistinguishable from a cap someone chose, so the only way to
/// know a config survived a round trip is to compare it, field by field, against the one it came
/// from: `assert!(dag_config_carry_diff(&loaded, &rebuilt).is_empty())`.
///
/// Deliberately NOT a derived `PartialEq`. The comparison destructures both sides exhaustively,
/// so adding a field to `DagConfig` is a COMPILE ERROR here until its comparison is written —
/// which is exactly this bug's shape, a new field quietly defaulting at a call site nobody
/// revisited. A derived `PartialEq` would start silently covering new fields and then, the first
/// time one was `f64::NAN` or intentionally excluded, silently stop being an assertion at all.
///
/// `steps` are compared by tag sequence, not deeply: this answers "did the POLICY survive", and a
/// consumer that rebuilds a config is by construction keeping the same steps.
pub fn dag_config_carry_diff(from: &DagConfig, to: &DagConfig) -> Vec<String> {
    // Exhaustive destructuring, NO `..`: this is the compile-time half of the guarantee.
    let DagConfig {
        steps: from_steps,
        description: from_description,
        resource_caps: from_resource_caps,
        mem_cap_factor: from_mem_cap_factor,
        mem_cap_floor_bytes: from_mem_cap_floor_bytes,
        outer_mem_safety_factor: from_outer_mem_safety_factor,
        default_step_timeout: from_default_step_timeout,
        default_jobs_flag: from_default_jobs_flag,
        default_jobs_env: from_default_jobs_env,
        default_step_mem_cap_bytes: from_default_step_mem_cap_bytes,
        default_step_cpu_count: from_default_step_cpu_count,
        default_step_cpu_timeout: from_default_step_cpu_timeout,
        cpu_timeout_multiplier: from_cpu_timeout_multiplier,
        cpu_timeout_platform: from_cpu_timeout_platform,
        write_domain_policy: from_write_domain_policy,
    } = from;
    let DagConfig {
        steps: to_steps,
        description: to_description,
        resource_caps: to_resource_caps,
        mem_cap_factor: to_mem_cap_factor,
        mem_cap_floor_bytes: to_mem_cap_floor_bytes,
        outer_mem_safety_factor: to_outer_mem_safety_factor,
        default_step_timeout: to_default_step_timeout,
        default_jobs_flag: to_default_jobs_flag,
        default_jobs_env: to_default_jobs_env,
        default_step_mem_cap_bytes: to_default_step_mem_cap_bytes,
        default_step_cpu_count: to_default_step_cpu_count,
        default_step_cpu_timeout: to_default_step_cpu_timeout,
        cpu_timeout_multiplier: to_cpu_timeout_multiplier,
        cpu_timeout_platform: to_cpu_timeout_platform,
        write_domain_policy: to_write_domain_policy,
    } = to;

    let mut out: Vec<String> = Vec::new();
    let mut note = |field: &str, a: String, b: String| {
        if a != b {
            out.push(format!("{field}: {a} -> {b}"));
        }
    };
    let tags = |steps: &[Step]| steps.iter().map(Step::tag).collect::<Vec<_>>().join(",");
    note("steps", tags(from_steps), tags(to_steps));
    note(
        "description",
        from_description.clone(),
        to_description.clone(),
    );
    note(
        "resource_caps",
        render_caps(from_resource_caps),
        render_caps(to_resource_caps),
    );
    // Rendered, not compared as f64: NaN != NaN would report an unchanged field as dropped, and
    // a report that fires on a config nobody touched is a report nobody reads.
    note(
        "mem_cap_factor",
        from_mem_cap_factor.to_string(),
        to_mem_cap_factor.to_string(),
    );
    note(
        "mem_cap_floor_bytes",
        from_mem_cap_floor_bytes.to_string(),
        to_mem_cap_floor_bytes.to_string(),
    );
    note(
        "outer_mem_safety_factor",
        from_outer_mem_safety_factor.to_string(),
        to_outer_mem_safety_factor.to_string(),
    );
    note(
        "default_step_timeout",
        from_default_step_timeout.to_string(),
        to_default_step_timeout.to_string(),
    );
    note(
        "default_jobs_flag",
        from_default_jobs_flag.clone(),
        to_default_jobs_flag.clone(),
    );
    note(
        "default_jobs_env",
        from_default_jobs_env.clone(),
        to_default_jobs_env.clone(),
    );
    note(
        "default_step_mem_cap_bytes",
        render_opt_int(*from_default_step_mem_cap_bytes),
        render_opt_int(*to_default_step_mem_cap_bytes),
    );
    note(
        "default_step_cpu_count",
        render_opt_int(*from_default_step_cpu_count),
        render_opt_int(*to_default_step_cpu_count),
    );
    note(
        "default_step_cpu_timeout",
        from_default_step_cpu_timeout.to_string(),
        to_default_step_cpu_timeout.to_string(),
    );
    note(
        "cpu_timeout_multiplier",
        from_cpu_timeout_multiplier.to_string(),
        to_cpu_timeout_multiplier.to_string(),
    );
    note(
        "cpu_timeout_platform",
        from_cpu_timeout_platform.clone(),
        to_cpu_timeout_platform.clone(),
    );
    note(
        "write_domain_policy",
        render_policy(from_write_domain_policy),
        render_policy(to_write_domain_policy),
    );
    out
}

fn render_caps(caps: &BTreeMap<String, i64>) -> String {
    let body = caps
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{body}}}")
}

/// `None` renders as `<absent>`, never as `0`: ABSENT IS NOT ZERO here either, and a disabled
/// default cap and a cap of zero are opposite instructions.
fn render_opt_int(value: Option<i64>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "<absent>".to_string(),
    }
}

fn render_policy(policy: &WriteDomainPolicy) -> String {
    let domains = policy
        .allowed_domains
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "require_explicit={} allowed=[{}]",
        policy.require_explicit, domains
    )
}

/// Steps demanding a named resource that `resource_caps` never declares.
///
/// ABSENT IS NOT ZERO, and this is the one place the difference can still be seen. The
/// scheduler's gate reads `resource_avail.get(name).unwrap_or(0)`, so an undeclared resource and
/// a resource deliberately capped at 0 collapse into the same integer and produce byte-identical
/// behaviour: the step is never ready, the ready-set loop keeps sleeping, and the run sits at 0%
/// CPU emitting nothing until some outer deadline kills it. Their remedies are opposites —
/// "declare the capacity you forgot" versus "this is blocked on purpose" — so a report that
/// cannot tell them apart is worse than no report.
///
/// Only a demand GREATER THAN ZERO can starve, and an intentionally-skipped step never launches,
/// so neither is named here. A cap DECLARED as 0 is a real value and is likewise not named: it
/// still gates the step, exactly as its author asked.
///
/// Returns sorted `"<tag>: <resource>"` entries, empty when every demand has a declared cap.
pub fn undeclared_resource_demands(cfg: &DagConfig) -> Vec<String> {
    let mut out: Vec<String> = cfg
        .steps
        .iter()
        .filter(|s| s.skip_reason.is_none())
        .flat_map(|s| {
            let tag = s.tag();
            s.hint
                .resources
                .iter()
                .filter(|(name, count)| **count > 0 && !cfg.resource_caps.contains_key(*name))
                .map(move |(name, _)| format!("{tag}: {name}"))
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Ways the GRAPH ITSELF cannot mean what it says, named before anything runs.
///
/// Each entry describes a graph whose declared steps cannot all be executed in the order the
/// graph asks for. None of them is a matter of taste.
///
/// DUPLICATE TAG is the worst of the set, because it is the one that stays SILENT. Two steps
/// declared with the same `group.job` collapse into one entry in every `by_tag` index the runner
/// builds, so exactly one of them ever runs, the other vanishes without a word, and the summary
/// still counts both as passed. A run that reports "2 passed" having executed one command is not
/// a partial failure; it is a false report.
///
/// MISSING DEPENDENCY names a predecessor no step declares. Left to the scheduler this is a
/// "terminal starve" discovered only after every unrelated step has already run, so a typo in one
/// edge costs a full build before it is reported.
///
/// CYCLE is the crash. Nothing downstream of the loader tolerates one: the bottom-level walk
/// recurses along the cycle until the stack is exhausted and the process ABORTS WITH A CORE DUMP,
/// and the critical-path walk never reaches a sink. Those walks are written for an acyclic graph
/// on purpose; this is the check that makes that assumption true. The refusal NAMES the cycle,
/// because "there is a cycle" in a 200-node graph is not actionable.
///
/// UNSATISFIABLE RESOURCE DEMAND is a step whose demand exceeds a POSITIVE declared cap, so it can
/// never be admitted however long the run waits. A cap declared as exactly `0` is deliberately NOT
/// included: `0` means "blocked on purpose" (see [`undeclared_resource_demands`]), and a check
/// here would turn that documented affordance into a load error.
///
/// Returns human-readable entries in a deterministic order, empty when the graph is sound.
/// Duplicate tags SHORT-CIRCUIT the remaining checks: while two steps share a tag, every statement
/// about "the step named X" is ambiguous, and reporting edges against an arbitrary winner would be
/// guesswork presented as fact.
pub fn graph_structure_violations(cfg: &DagConfig) -> Vec<String> {
    // Both language editions of this runner must refuse the same graphs with the same bytes, so
    // the message text and the traversal order here are a shared contract, pinned by the
    // cross-language differential.
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for step in &cfg.steps {
        *counts.entry(step.tag()).or_insert(0) += 1;
    }
    let duplicates: Vec<String> = counts
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(tag, n)| {
            format!(
                "duplicate step tag '{tag}': declared {n} times, but a tag names exactly one \
                 step -- only ONE of them would ever run and the rest would vanish silently"
            )
        })
        .collect();
    if !duplicates.is_empty() {
        return duplicates;
    }

    let mut bad: Vec<String> = Vec::new();
    for step in &cfg.steps {
        let missing: BTreeSet<&String> = step
            .deps
            .iter()
            .filter(|dep| !counts.contains_key(*dep))
            .collect();
        for dep in missing {
            bad.push(format!(
                "step {}: depends on '{dep}', which no step declares",
                step.tag()
            ));
        }
    }

    // Iterative three-colour DFS over the dependency relation, matching
    // `refuse_unusable_explains`: iterative so a deep chain cannot blow the stack and turn a
    // validation error into the very crash this check exists to prevent. Only edges that RESOLVE
    // are followed, so a missing dependency is reported once (above) rather than also
    // masquerading as a broken cycle.
    let by_tag: BTreeMap<String, &Step> = cfg.steps.iter().map(|s| (s.tag(), s)).collect();
    #[derive(Clone, Copy, PartialEq)]
    enum Colour {
        White,
        Grey,
        Black,
    }
    let mut colour: BTreeMap<&str, Colour> =
        by_tag.keys().map(|k| (k.as_str(), Colour::White)).collect();
    for root in by_tag.keys() {
        if colour[root.as_str()] != Colour::White {
            continue;
        }
        let mut stack: Vec<(&str, bool)> = vec![(root.as_str(), false)];
        let mut path: Vec<&str> = Vec::new();
        while let Some((tag, leaving)) = stack.pop() {
            if leaving {
                colour.insert(tag, Colour::Black);
                path.pop();
                continue;
            }
            match colour[tag] {
                Colour::Black => continue,
                Colour::Grey => {
                    let start = path.iter().position(|t| *t == tag).unwrap_or(0);
                    let mut cycle: Vec<&str> = path[start..].to_vec();
                    cycle.push(tag);
                    bad.push(format!("dependency cycle: {}", cycle.join(" -> ")));
                    // ONE cycle per root, then abandon this root entirely: every node still on
                    // the path is retired to Black so the outer loop cannot re-enter it and
                    // report the same cycle again from a second entry point. A cycle elsewhere in
                    // the graph is still reached from its own root.
                    for pending in &path {
                        colour.insert(pending, Colour::Black);
                    }
                    stack.clear();
                    break;
                }
                Colour::White => {}
            }
            colour.insert(tag, Colour::Grey);
            path.push(tag);
            stack.push((tag, true));
            let deps: BTreeSet<&str> = by_tag[tag].deps.iter().map(|s| s.as_str()).collect();
            for dep in deps {
                if by_tag.contains_key(dep) && colour.get(dep) != Some(&Colour::Black) {
                    stack.push((dep, false));
                }
            }
        }
    }

    for step in &cfg.steps {
        if step.skip_reason.is_some() {
            continue;
        }
        for (name, count) in &step.hint.resources {
            if let Some(cap) = cfg.resource_caps.get(name) {
                if *cap > 0 && *count > *cap {
                    bad.push(format!(
                        "step {}: demands {name}={count} but resource_caps declares \
                         {name}={cap}, so it can never be admitted",
                        step.tag()
                    ));
                }
            }
        }
    }

    for step in &cfg.steps {
        if let Err(error) = step.structured_test_results_manifest() {
            bad.push(error);
        }
    }
    bad
}

/// Return deterministic fail-closed write-domain declaration errors.
///
/// Parsers call this while loading, and scheduler entry points call it again so an
/// in-memory `DagConfig` cannot bypass the file parser.
pub fn write_domain_violations(cfg: &DagConfig) -> Vec<String> {
    let policy = &cfg.write_domain_policy;
    if !policy.require_explicit && policy.allowed_domains.is_empty() {
        return Vec::new();
    }
    let mut bad = Vec::new();
    let by_tag = cfg.by_tag();
    for step in &cfg.steps {
        let Some(domains) = &step.write_domains else {
            if policy.require_explicit {
                bad.push(format!(
                    "{}: missing write_domains (use [] for no protected domains)",
                    step.tag()
                ));
            }
            if step.write_domain_guarantee.is_some() {
                bad.push(format!(
                    "{}: write_domain_guarantee requires write_domains",
                    step.tag()
                ));
            }
            continue;
        };
        let mut seen = BTreeSet::new();
        let duplicates: BTreeSet<String> = domains
            .iter()
            .filter(|name| !seen.insert((*name).clone()))
            .cloned()
            .collect();
        if !duplicates.is_empty() {
            bad.push(format!(
                "{}: duplicate write_domains: {}",
                step.tag(),
                duplicates.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        let unknown: Vec<String> = domains
            .iter()
            .filter(|name| !policy.allowed_domains.contains(*name))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !unknown.is_empty() {
            bad.push(format!(
                "{}: unknown write_domains: {}",
                step.tag(),
                unknown.join(", ")
            ));
        }
        if !domains.is_empty() && step.write_domain_guarantee.is_none() {
            bad.push(format!(
                "{}: nonempty write_domains require write_domain_guarantee",
                step.tag()
            ));
        }
        if domains.is_empty() && step.write_domain_guarantee.is_some() {
            bad.push(format!(
                "{}: write_domains=[] cannot claim a write guarantee",
                step.tag()
            ));
        }
        if step.write_domain_guarantee == Some(WriteDomainGuarantee::ArtifactBarrierDependent) {
            let mut pending = step.deps.clone();
            let mut seen = BTreeSet::new();
            let mut found = false;
            while let Some(tag) = pending.pop() {
                if !seen.insert(tag.clone()) {
                    continue;
                }
                let Some(ancestor) = by_tag.get(&tag) else {
                    continue;
                };
                if ancestor.write_domain_guarantee
                    == Some(WriteDomainGuarantee::ImmutableArtifactBarrier)
                {
                    found = true;
                    break;
                }
                pending.extend(ancestor.deps.iter().cloned());
            }
            if !found {
                bad.push(format!(
                    "{}: artifact-barrier-dependent but no transitive dependency is an \
                     immutable-artifact-barrier",
                    step.tag()
                ));
            }
        }
    }
    bad
}

/// Terminal result of one scheduled step.
#[derive(Debug, Clone)]
pub struct StepOutcome {
    /// The step's tag (`"group.job"`).
    pub tag: String,
    /// Whether the step succeeded (exit 0, not timed out).
    pub ok: bool,
    /// Wall-clock seconds the step ran.
    pub duration_s: f64,
    /// One-line summary extracted from the step's output (`""` when unavailable).
    pub summary: String,
    /// Tests executed according to the step's complete captured runner output.
    /// `None` means no recognizable test-runner banner, distinct from `Some(0)`.
    pub executed_tests: Option<u64>,
    /// Tests filtered according to the same complete captured output.
    pub filtered_tests: Option<u64>,
    /// Terminal per-test results from a controlled runner. `None` means they were not recorded.
    pub test_results: Option<Vec<TestResult>>,
    /// Required-result refusal, preserved separately from any outer failure cause.
    pub test_results_error: Option<String>,
    /// Actual import operation that failed; absent for older or unclassified evidence.
    pub test_results_error_kind: Option<TestResultsErrorKind>,
    /// Child process exit code; negative for a Unix signal; `None` if never collected.
    pub returncode: Option<i64>,
    /// Whether this step or one of its descendants hit the step's inner memory limit.
    ///
    /// Kept separately from `reason`: the display string follows a precedence rule, so an OOM
    /// can otherwise hide a simultaneous wall- or CPU-timeout from typed consumers.
    pub oomed: bool,
    /// Number of `oom_kill` events observed for this step's inner cgroup.
    pub oom_kills: i64,
    /// Whether the step exceeded its wall-clock bound.
    pub timed_out: bool,
    /// Whether the step exceeded its CPU-time bound.
    pub cpu_timed_out: bool,
    /// Human-readable failure reason; `""` when `ok`.
    pub reason: String,
    /// True when eager-exit killed this in-flight step after ANOTHER step failed.
    pub aborted: bool,
}

impl StepOutcome {
    /// Build a passing outcome.
    pub fn passed(
        tag: String,
        duration_s: f64,
        summary: String,
        returncode: Option<i64>,
        executed_tests: Option<u64>,
        filtered_tests: Option<u64>,
    ) -> Self {
        StepOutcome {
            tag,
            ok: true,
            duration_s,
            summary,
            executed_tests,
            filtered_tests,
            test_results: None,
            test_results_error: None,
            test_results_error_kind: None,
            returncode,
            oomed: false,
            oom_kills: 0,
            timed_out: false,
            cpu_timed_out: false,
            reason: String::new(),
            aborted: false,
        }
    }

    /// Build a failed outcome, deriving `reason` from the shared precedence rule.
    #[allow(clippy::too_many_arguments)]
    pub fn failed(
        tag: String,
        duration_s: f64,
        summary: String,
        returncode: Option<i64>,
        oomed: bool,
        oom_kills: i64,
        timed_out: bool,
        timeout: i64,
        cpu_timed_out: bool,
        cpu_timeout: i64,
        cpu_timeout_canonical: i64,
        cpu_timeout_multiplier: f64,
        cpu_timeout_platform: &str,
        aborted: bool,
        executed_tests: Option<u64>,
        filtered_tests: Option<u64>,
    ) -> Self {
        let reason = step_failure_reason(
            returncode,
            oomed,
            oom_kills,
            timed_out,
            timeout,
            false,
            None,
            &[],
            // The Rust scheduler now enforces per-step CPU-time budgets under boxing,
            // at parity with the Python runner; thread the real breach flag through.
            cpu_timed_out,
            cpu_timeout,
            cpu_timeout_canonical,
            cpu_timeout_multiplier,
            cpu_timeout_platform,
        );
        StepOutcome {
            tag,
            ok: false,
            duration_s,
            summary,
            executed_tests,
            filtered_tests,
            test_results: None,
            test_results_error: None,
            test_results_error_kind: None,
            returncode,
            oomed,
            oom_kills,
            timed_out,
            cpu_timed_out,
            reason,
            aborted,
        }
    }

    /// Build an ABORTED outcome (a cancellation, not a genuine failure).
    ///
    /// `cut_by_run_budget` picks which of the two cancellations this was. They call for
    /// completely different follow-up, and the terminal output has always distinguished them
    /// while this `reason` did not: it said eager-exit unconditionally, so a run-budget cut
    /// recorded a failing peer that does not exist. A reader who only has the record — which is
    /// the reader this field exists for — was then sent hunting for it.
    pub fn aborted_outcome(
        tag: String,
        duration_s: f64,
        summary: String,
        returncode: Option<i64>,
        executed_tests: Option<u64>,
        filtered_tests: Option<u64>,
        cut_by_run_budget: bool,
    ) -> Self {
        StepOutcome {
            tag,
            ok: false,
            duration_s,
            summary,
            executed_tests,
            filtered_tests,
            test_results: None,
            test_results_error: None,
            test_results_error_kind: None,
            returncode,
            oomed: false,
            oom_kills: 0,
            timed_out: false,
            cpu_timed_out: false,
            reason: if cut_by_run_budget {
                ABORTED_BY_RUN_BUDGET_REASON.to_string()
            } else {
                ABORTED_BY_PEER_FAILURE_REASON.to_string()
            },
            aborted: true,
        }
    }
}

/// Cancelled because the whole run exhausted its outer budget, not by any failure.
///
/// Kept beside its sibling below so the two cancellations cannot drift into wording that reads
/// the same. The clause after the comma is the part that matters to a reader deciding whether to
/// go looking for a failing step.
pub const ABORTED_BY_RUN_BUDGET_REASON: &str =
    "ABORTED (cut short by the OUTER run budget, not by a failure of its own or of a peer)";

/// Cancelled by eager-exit after a peer step failed.
pub const ABORTED_BY_PEER_FAILURE_REASON: &str =
    "ABORTED (eager-exit after another step failed; --keep-going would continue independent work)";

/// Aggregate outcome of a whole DAG run.
#[derive(Debug, Clone, Default)]
pub struct RunResult {
    /// Overall pass/fail (no genuine, non-aborted failure occurred).
    pub ok: bool,
    /// Wall-clock seconds the whole run took.
    pub wall_s: f64,
    /// Per-step terminal results, in dispatch (LPT) order.
    pub outcomes: Vec<StepOutcome>,
    /// Tags whose dependencies failed so they never ran (sorted).
    pub skipped: Vec<String>,
    /// Configured tags with no terminal outcome that were NOT dependency-skipped and NOT
    /// intentionally skipped: work the run never got to, because fail-fast tripped or the outer
    /// run budget cut the run short. Its own bucket so absent work can never be mistaken for
    /// passing work by a consumer that only counts outcomes (sorted).
    pub not_launched: Vec<String>,
    /// `(tag, stable reason)` for nodes deliberately omitted before process spawn.
    pub intentional_skips: Vec<(String, IntentionalSkipReason)>,
    /// Per-step measurement rows (column -> value) to forward to a metrics sink; empty when no
    /// cgroup manager supplied per-step metrics.
    pub step_profile_rows: Vec<BTreeMap<String, String>>,
    /// Opt-in per-step CPU/thread samples, ordered by step completion and then sample index.
    /// Empty when time-series profiling was not requested.
    pub step_timeseries_rows: Vec<BTreeMap<String, String>>,
    /// The WHOLE RUN hit an outer wall or CPU budget, or lost required CPU accounting, and was
    /// cut short.
    ///
    /// Distinct from a step's own `timed_out`: no single node necessarily misbehaved, the
    /// combination did. A consumer that records results must be able to tell "this run produced a
    /// verdict about the tree" from "this run was stopped by its own budget with work still
    /// outstanding", and `ok == false` alone cannot.
    pub run_timed_out: bool,
    /// The WHOLE RUN crossed its cumulative CPU-time budget.
    ///
    /// `run_timed_out` is also set. This narrower bit lets callers distinguish consumed CPU from
    /// elapsed wall time instead of reporting the two quantities as interchangeable.
    pub run_cpu_timed_out: bool,
    /// Required whole-run CPU accounting became unreadable after execution started.
    ///
    /// This is fail-closed and also sets `run_timed_out`, because continuing would turn a named
    /// CPU cap into an unenforced claim.
    pub run_cpu_accounting_failed: bool,
    /// Largest number of successfully spawned step children whose exit observation remains
    /// pending. Spawn/count and exit-observation/uncount are serialized, so neither boundary can
    /// be reordered around the other. A kernel exit remains counted until its supervisor observes
    /// it; this is an observation-order high-water mark, not an inferred scheduler-admission count.
    pub max_concurrent_steps: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_enables_small_forcing_caps() {
        let cfg = DagConfig::default();
        assert_eq!(
            cfg.default_step_mem_cap_bytes,
            Some(DEFAULT_SMALL_MEM_CAP_BYTES)
        );
        assert_eq!(cfg.default_step_cpu_count, Some(DEFAULT_SMALL_CPU_COUNT));
        assert_eq!(cfg.default_step_cpu_timeout, DEFAULT_SMALL_CPU_TIMEOUT);
    }

    #[test]
    fn classification_browser_promotes_to_latency() {
        let mut hint = ResourceHint::default();
        hint.resources.insert("browser".to_string(), 1);
        let step = Step {
            group: "e2e".into(),
            job: "smoke".into(),
            desc: String::new(),
            description: String::new(),
            labels: Vec::new(),
            cmd: "true".into(),
            cmdtype: CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: vec![],
            env: BTreeMap::new(),
            hint,
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: DEFAULT_STEP_TIMEOUT,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        };
        assert_eq!(step_classification(&step), StepClass::LatencyBound);
    }

    fn bare_step(cmd: &str, jobs_flag: Option<&str>) -> Step {
        Step {
            group: "g".into(),
            job: "j".into(),
            desc: String::new(),
            description: String::new(),
            labels: Vec::new(),
            cmd: cmd.into(),
            cmdtype: CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: vec![],
            env: BTreeMap::new(),
            hint: ResourceHint::default(),
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: DEFAULT_STEP_TIMEOUT,
            cpu_timeout: 0,
            jobs_flag: jobs_flag.map(str::to_string),
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        }
    }

    fn exact_result() -> DagManifest {
        DagManifest {
            lane: "portable".into(),
            category: "applications".into(),
            test: Some("applications/date".into()),
            mode: Some("verify".into()),
            backend: Some("ptrace".into()),
        }
    }

    #[test]
    fn result_manifest_owner_preserves_fallback_and_explicit_empty() {
        let legacy = Step {
            group: "e2e".into(),
            job: "legacy".into(),
            manifest: Some(DagManifest {
                lane: "portable".into(),
                category: "applications".into(),
                test: None,
                mode: None,
                backend: None,
            }),
            ..bare_step("true", None)
        };
        assert_eq!(
            result_manifest_owner(std::slice::from_ref(&legacy), &exact_result())
                .unwrap()
                .tag(),
            "e2e.legacy"
        );

        let explicit_empty = Step {
            result_manifests: Some(Vec::new()),
            ..legacy.clone()
        };
        let error = result_manifest_owner(&[explicit_empty], &exact_result()).unwrap_err();
        assert!(error.contains("has no owning step"), "{error}");
    }

    #[test]
    fn result_manifest_owner_refuses_inexact_missing_and_cross_node_ownership() {
        let broad = Step {
            group: "e2e".into(),
            job: "broad".into(),
            result_manifests: Some(vec![DagManifest {
                lane: "portable".into(),
                category: "applications".into(),
                test: None,
                mode: Some("verify".into()),
                backend: Some("ptrace".into()),
            }
            .into()]),
            ..bare_step("true", None)
        };

        let inexact = DagManifest {
            lane: "portable".into(),
            category: "applications".into(),
            test: None,
            mode: None,
            backend: None,
        };
        let error = result_manifest_owner(std::slice::from_ref(&broad), &inexact).unwrap_err();
        assert!(error.contains("missing test, mode, backend"), "{error}");

        for (empty, missing) in [
            (
                DagManifest {
                    lane: String::new(),
                    ..exact_result()
                },
                "lane",
            ),
            (
                DagManifest {
                    category: String::new(),
                    ..exact_result()
                },
                "category",
            ),
        ] {
            let error = result_manifest_owner(std::slice::from_ref(&broad), &empty).unwrap_err();
            assert!(error.contains(&format!("missing {missing}")), "{error}");
        }

        let missing = DagManifest {
            category: "c-programs".into(),
            ..exact_result()
        };
        let error = result_manifest_owner(std::slice::from_ref(&broad), &missing).unwrap_err();
        assert!(error.contains("has no owning step"), "{error}");

        let exact = Step {
            group: "e2e".into(),
            job: "exact".into(),
            result_manifests: Some(vec![exact_result().into()]),
            ..bare_step("true", None)
        };

        let structured = Step {
            group: "test".into(),
            job: "counts".into(),
            result_manifests: Some(vec![ResultManifest::StructuredTestResults(
                StructuredTestResultsManifest::current("test.counts"),
            )]),
            ..bare_step("true", None)
        };
        assert!(structured.effective_result_manifests().is_empty());
        assert_eq!(
            structured
                .structured_test_results_manifest()
                .expect("structured declaration")
                .expect("present structured declaration")
                .owner,
            "test.counts"
        );
        let error = result_manifest_owner(&[broad, exact], &exact_result()).unwrap_err();
        assert!(
            error.contains("multiple owning steps in the selected DAG: e2e.broad, e2e.exact"),
            "{error}"
        );
    }

    #[test]
    fn structured_result_manifest_owner_and_cardinality_are_structural() {
        let declaration = |owner: &str| {
            ResultManifest::StructuredTestResults(StructuredTestResultsManifest::current(owner))
        };
        let wrong_owner = Step {
            group: "test".into(),
            job: "counts".into(),
            result_manifests: Some(vec![declaration("other.step")]),
            ..bare_step("true", None)
        };
        assert_eq!(
            graph_structure_violations(&DagConfig {
                steps: vec![wrong_owner],
                ..DagConfig::default()
            }),
            vec![
                "step test.counts: structured test-result owner 'other.step' must equal the containing step tag"
                    .to_string()
            ]
        );

        let duplicate = Step {
            group: "test".into(),
            job: "counts".into(),
            result_manifests: Some(vec![declaration("test.counts"), declaration("test.counts")]),
            ..bare_step("true", None)
        };
        assert_eq!(
            graph_structure_violations(&DagConfig {
                steps: vec![duplicate],
                ..DagConfig::default()
            }),
            vec!["step test.counts: declares structured test results more than once".to_string()]
        );
    }

    #[test]
    fn render_jobs_flag_forms() {
        assert_eq!(render_jobs_flag("-j", 4), "-j 4");
        assert_eq!(render_jobs_flag("-j%d", 4), "-j4");
        assert_eq!(render_jobs_flag("--jobs=", 8), "--jobs=8");
        assert_eq!(render_jobs_flag("--num-threads", 2), "--num-threads 2");
        assert_eq!(render_jobs_flag("--threads=%d", 3), "--threads=3");
    }

    #[test]
    fn command_with_inner_jobs_appends_and_respects_defaults() {
        // No inner jobs -> unchanged.
        let s = bare_step("make", None);
        assert_eq!(command_with_inner_jobs(&s, "-j", None), "make");
        // Inner jobs + default template.
        assert_eq!(command_with_inner_jobs(&s, "-j", Some(4)), "make -j 4");
        // Step-level jobs_flag overrides the default.
        let s2 = bare_step("cargo build", Some("-j%d"));
        assert_eq!(
            command_with_inner_jobs(&s2, "-j", Some(8)),
            "cargo build -j8"
        );
        // Empty template disables appending.
        let s3 = bare_step("mytool", Some(""));
        assert_eq!(command_with_inner_jobs(&s3, "-j", Some(4)), "mytool");
        let s4 = bare_step("fixed", Some("   "));
        assert_eq!(command_with_inner_jobs(&s4, "-j", Some(4)), "fixed");
    }

    #[test]
    fn jobs_env_resolves_overrides_and_marks_env_only_steps_resizable() {
        assert_eq!(
            resolve_jobs_env(Some(" CARGO_BUILD_JOBS ")).unwrap(),
            "CARGO_BUILD_JOBS"
        );
        assert!(resolve_jobs_env(Some("not a name")).is_err());

        let mut step = bare_step("cargo build", Some(""));
        assert!(!step_width_is_resizable(&step, "-j", ""));
        assert!(step_width_is_resizable(&step, "-j", "CARGO_BUILD_JOBS"));
        assert_eq!(
            env_with_inner_jobs(&step, "CARGO_BUILD_JOBS", Some(4)),
            Some(("CARGO_BUILD_JOBS".to_string(), "4".to_string()))
        );

        step.jobs_env = Some("MAKEFLAGS_J".to_string());
        assert_eq!(effective_jobs_env(&step, "CARGO_BUILD_JOBS"), "MAKEFLAGS_J");
        assert_eq!(
            env_with_inner_jobs(&step, "CARGO_BUILD_JOBS", Some(2)),
            Some(("MAKEFLAGS_J".to_string(), "2".to_string()))
        );
        step.jobs_env = Some(String::new());
        assert!(!step_width_is_resizable(&step, "-j", "CARGO_BUILD_JOBS"));
    }

    #[test]
    fn jobs_env_shell_control_names_are_refused_without_replacing_runner_authority() {
        let names = [
            "BASH_COMPAT",
            "BASH_ENV",
            "BASH_XTRACEFD",
            "CDPATH",
            "ENV",
            "EXECIGNORE",
            "GLOBIGNORE",
            "PATH",
            "POSIXLY_CORRECT",
        ];
        let accepted: Vec<_> = names
            .iter()
            .filter(|name| resolve_jobs_env(Some(name)).is_ok())
            .collect();
        assert!(
            accepted.is_empty(),
            "accepted shell-control channels: {accepted:?}"
        );
        for name in names {
            assert!(resolve_jobs_env(Some(name))
                .unwrap_err()
                .contains("shell startup/control variable"));
            for doc in [
                serde_json::json!({"default_jobs_env": name, "steps": []}),
                serde_json::json!({"steps": [{"group":"g", "job":"j", "cmd":"true", "jobs_flag":"-j", "jobs_env":name}]}),
            ] {
                assert!(crate::io::dag_from_json(&doc.to_string())
                    .unwrap_err()
                    .0
                    .contains("shell startup/control variable"));
            }
        }
        for name in [
            "DAGRUN_DELEGATED_CGROUP",
            "DAGRUN_DELEGATED_UNBOXED",
            "DAGRUN_EXTRA_ARGS",
            "DAGRUN_OUTER_RUN",
            "DAGRUN_STEP",
        ] {
            assert!(resolve_jobs_env(Some(name))
                .unwrap_err()
                .contains("reserved by dagrun"));
        }
    }

    #[test]
    fn malformed_programmatic_jobs_env_is_refused() {
        let mut cfg = DagConfig {
            default_jobs_env: "bad=name".to_string(),
            ..Default::default()
        };
        assert!(validate_jobs_env_config(&cfg).is_err());
        cfg.default_jobs_env.clear();
        let mut step = bare_step("true", None);
        step.jobs_env = Some("not a name".to_string());
        cfg.steps = vec![step];
        assert!(validate_jobs_env_config(&cfg).is_err());
        assert!(std::panic::catch_unwind(|| {
            step_width_is_resizable(&cfg.steps[0], &cfg.default_jobs_flag, &cfg.default_jobs_env)
        })
        .is_err());
        for reserved in [
            "DAGRUN_DELEGATED_CGROUP",
            "DAGRUN_DELEGATED_UNBOXED",
            "DAGRUN_EXTRA_ARGS",
            "DAGRUN_OUTER_RUN",
            "DAGRUN_STEP",
        ] {
            cfg.default_jobs_env = reserved.to_string();
            cfg.steps.clear();
            assert!(
                validate_jobs_env_config(&cfg).is_err(),
                "runner-owned environment name {reserved} was accepted as a jobs channel"
            );
        }
    }

    #[test]
    fn failure_reason_precedence() {
        // OOM beats a signal.
        assert_eq!(
            step_failure_reason(
                Some(-9),
                true,
                2,
                false,
                10,
                false,
                None,
                &[],
                false,
                0,
                0,
                DEFAULT_CPU_TIMEOUT_MULTIPLIER,
                ""
            ),
            "OOM-KILLED (hit inner MemoryMax; 2 oom_kill event(s))"
        );
        // CPU-timeout beats a wall timeout (more specific cause).
        assert_eq!(
            step_failure_reason(
                Some(-9),
                false,
                0,
                true,
                600,
                false,
                None,
                &[],
                true,
                30,
                0,
                DEFAULT_CPU_TIMEOUT_MULTIPLIER,
                ""
            ),
            "CPU-TIMEOUT >30s cpu"
        );
        // timeout beats a signal.
        assert_eq!(
            step_failure_reason(
                Some(-15),
                false,
                0,
                true,
                30,
                false,
                None,
                &[],
                false,
                0,
                0,
                DEFAULT_CPU_TIMEOUT_MULTIPLIER,
                ""
            ),
            "TIMEOUT >30s"
        );
        // negative return code without oom/timeout -> signal name.
        assert_eq!(
            step_failure_reason(
                Some(-9),
                false,
                0,
                false,
                10,
                false,
                None,
                &[],
                false,
                0,
                0,
                DEFAULT_CPU_TIMEOUT_MULTIPLIER,
                ""
            ),
            "received SIGKILL with no validate timeout, pids guard, \
             or child-cgroup OOM recorded"
        );
        // plain non-zero exit.
        assert_eq!(
            step_failure_reason(
                Some(1),
                false,
                0,
                false,
                10,
                false,
                None,
                &[],
                false,
                0,
                0,
                DEFAULT_CPU_TIMEOUT_MULTIPLIER,
                ""
            ),
            "exit 1"
        );
    }

    #[test]
    fn step_outcome_retains_every_failure_fact_hidden_by_reason_precedence() {
        let outcome = StepOutcome::failed(
            "g.j".into(),
            1.0,
            String::new(),
            Some(-9),
            true,
            3,
            true,
            30,
            true,
            10,
            10,
            DEFAULT_CPU_TIMEOUT_MULTIPLIER,
            "",
            false,
            None,
            None,
        );
        assert!(
            outcome.reason.starts_with("OOM-KILLED"),
            "{}",
            outcome.reason
        );
        assert!(
            outcome.oomed,
            "OOM must remain typed when it wins the display precedence"
        );
        assert_eq!(outcome.oom_kills, 3);
        assert!(
            outcome.timed_out,
            "wall timeout must not vanish behind the OOM message"
        );
        assert!(
            outcome.cpu_timed_out,
            "CPU timeout must not vanish behind the OOM message"
        );
    }
}

#[cfg(test)]
mod cpu_timeout_multiplier_tests {
    //! Per-platform CPU-budget multiplier contract.
    //!
    //! The scaled number and breach string must remain stable across implementations. The
    //! rounding case is not incidental: half-away-from-zero is required at every `.5` tie.
    use super::*;

    fn step(cpu_timeout: i64) -> Step {
        Step {
            group: "g".into(),
            job: "j".into(),
            desc: "d".into(),
            description: String::new(),
            labels: Vec::new(),
            cmd: "true".into(),
            cmdtype: CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: vec![],
            env: std::collections::BTreeMap::new(),
            hint: ResourceHint::default(),
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: DEFAULT_STEP_TIMEOUT,
            cpu_timeout,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        }
    }

    #[test]
    fn unity_is_a_strict_no_op() {
        assert_eq!(DEFAULT_CPU_TIMEOUT_MULTIPLIER, 1.0);
        let cfg = DagConfig::default();
        assert_eq!(cfg.cpu_timeout_multiplier, DEFAULT_CPU_TIMEOUT_MULTIPLIER);
        assert_eq!(cfg.cpu_timeout_platform, "");
        let s = step(30);
        assert_eq!(canonical_cpu_timeout(&s, DEFAULT_SMALL_CPU_TIMEOUT), 30);
        assert_eq!(
            effective_cpu_timeout(
                &s,
                DEFAULT_SMALL_CPU_TIMEOUT,
                DEFAULT_CPU_TIMEOUT_MULTIPLIER
            ),
            30
        );
    }

    #[test]
    fn declared_and_default_budgets_both_scale() {
        assert_eq!(
            effective_cpu_timeout(&step(30), DEFAULT_SMALL_CPU_TIMEOUT, 2.0),
            60
        );
        assert_eq!(
            effective_cpu_timeout(&step(30), DEFAULT_SMALL_CPU_TIMEOUT, 1.5),
            45
        );
        // The forcing-function floor is a budget like any other.
        assert_eq!(effective_cpu_timeout(&step(0), 10, 2.0), 20);
    }

    #[test]
    fn rounding_matches_python_half_away_from_zero() {
        // These exact pairs are asserted in the Python suite too. Banker's rounding would give
        // 4 / 10 / 8 / 2 / 14 for the .5 ties and diverge from this engine.
        assert_eq!(scale_cpu_timeout(3, 1.5), 5);
        assert_eq!(scale_cpu_timeout(7, 1.5), 11);
        assert_eq!(scale_cpu_timeout(5, 1.5), 8);
        assert_eq!(scale_cpu_timeout(1, 1.5), 2);
        assert_eq!(scale_cpu_timeout(9, 1.5), 14);
        assert_eq!(scale_cpu_timeout(3, 2.0), 6);
    }

    #[test]
    fn scaling_can_never_become_an_opt_out() {
        // A disabled budget stays disabled; multiplying must not invent a guard.
        assert_eq!(scale_cpu_timeout(0, 2.0), 0);
        assert_eq!(scale_cpu_timeout(-5, 2.0), 0);
        // A sub-unity multiplier must never round a live budget down to 0 (= guard removed).
        assert_eq!(scale_cpu_timeout(3, 0.1), 1);
        assert_eq!(scale_cpu_timeout(1, 0.01), 1);
    }

    #[test]
    fn scaled_breach_names_canonical_multiplier_and_platform() {
        let reason = step_failure_reason(
            Some(-9),
            false,
            0,
            false,
            600,
            false,
            None,
            &[],
            true,
            60,
            30,
            2.0,
            "github-hosted",
        );
        assert_eq!(
            reason,
            "CPU-TIMEOUT >60s cpu (canonical 30s x2 github-hosted)"
        );
    }

    #[test]
    fn breach_is_unchanged_at_unity_and_platform_label_is_optional() {
        let unscaled = step_failure_reason(
            Some(-9),
            false,
            0,
            false,
            600,
            false,
            None,
            &[],
            true,
            30,
            30,
            DEFAULT_CPU_TIMEOUT_MULTIPLIER,
            "github-hosted",
        );
        assert_eq!(unscaled, "CPU-TIMEOUT >30s cpu");
        let unlabelled = step_failure_reason(
            Some(-9),
            false,
            0,
            false,
            600,
            false,
            None,
            &[],
            true,
            45,
            30,
            1.5,
            "",
        );
        assert_eq!(unlabelled, "CPU-TIMEOUT >45s cpu (canonical 30s x1.5)");
    }

    #[test]
    fn oom_still_outranks_a_scaled_cpu_timeout() {
        let reason = step_failure_reason(
            Some(-9),
            true,
            2,
            false,
            600,
            false,
            None,
            &[],
            true,
            60,
            30,
            2.0,
            "github-hosted",
        );
        assert!(reason.starts_with("OOM-KILLED"), "{reason}");
    }

    #[test]
    fn a_nonpositive_explicit_multiplier_is_refused() {
        assert!(resolve_cpu_timeout_multiplier(Some(0.0)).is_err());
        assert!(resolve_cpu_timeout_multiplier(Some(-1.0)).is_err());
        assert_eq!(resolve_cpu_timeout_multiplier(Some(1.5)).unwrap().0, 1.5);
    }
}

/// The CARRY ASSERTION: a rebuilt `DagConfig` must be provably the same configuration.
#[cfg(test)]
mod carry_tests {
    use super::*;

    fn step() -> Step {
        Step {
            group: "g".into(),
            job: "j".into(),
            desc: "d".into(),
            description: String::new(),
            labels: Vec::new(),
            cmd: "true".into(),
            cmdtype: CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: vec![],
            env: BTreeMap::new(),
            hint: ResourceHint::default(),
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: DEFAULT_STEP_TIMEOUT,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        }
    }

    /// A config with every top-level field DELIBERATELY off its default, so a dropped field
    /// cannot coincide with the default it was replaced by. `mem_cap_factor` 1.25 and
    /// `outer_mem_safety_factor` 1.0 were harmless in the live bug only by that coincidence.
    fn configured() -> DagConfig {
        let mut caps = BTreeMap::new();
        caps.insert("widget_guest".to_string(), 1);
        caps.insert("manifest_guest".to_string(), 4);
        let policy = WriteDomainPolicy {
            require_explicit: true,
            allowed_domains: BTreeSet::from(["shared-cargo-target".to_string()]),
        };
        DagConfig {
            steps: vec![Step {
                write_domains: Some(vec!["shared-cargo-target".to_string()]),
                ..step()
            }],
            description: "a real lane".to_string(),
            resource_caps: caps,
            mem_cap_factor: 1.5,
            mem_cap_floor_bytes: 4 * 1024i64.pow(3),
            outer_mem_safety_factor: 1.2,
            default_step_timeout: 600,
            default_jobs_flag: "--jobs {n}".to_string(),
            default_jobs_env: "BUILD_JOBS".to_string(),
            default_step_mem_cap_bytes: None,
            default_step_cpu_count: Some(4),
            default_step_cpu_timeout: 120,
            cpu_timeout_multiplier: 2.0,
            cpu_timeout_platform: "github-hosted".to_string(),
            write_domain_policy: policy,
        }
    }

    #[test]
    fn carry_diff_is_empty_for_a_config_compared_with_itself() {
        let cfg = configured();
        assert_eq!(dag_config_carry_diff(&cfg, &cfg), Vec::<String>::new());
        assert_eq!(
            dag_config_carry_diff(&DagConfig::default(), &DagConfig::default()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn carry_diff_names_every_field_a_default_rebuild_drops() {
        let cfg = configured();
        // The exact footgun from the field: keep the steps, revert everything else.
        let rebuilt = DagConfig {
            steps: cfg.steps.clone(),
            ..Default::default()
        };
        let diff = dag_config_carry_diff(&cfg, &rebuilt);
        let named: Vec<&str> = diff
            .iter()
            .map(|line| line.split(':').next().unwrap())
            .collect();
        // Every field EXCEPT `steps` -- which is the one the literal named, and the only one
        // that survived. 13 of 14.
        let expected: Vec<&str> = DAG_CONFIG_FIELDS
            .iter()
            .copied()
            .filter(|f| *f != "steps")
            .collect();
        assert_eq!(named, expected, "carry diff: {diff:?}");
        // The loudest one in the live incident, spelled out: 600 s became 1800 s.
        assert!(
            diff.contains(&"default_step_timeout: 600 -> 0".to_string()),
            "{diff:?}"
        );
    }

    #[test]
    fn with_steps_carries_every_field_the_default_rebuild_drops() {
        let cfg = configured();
        let carried = cfg.with_steps(cfg.steps.clone());
        assert_eq!(dag_config_carry_diff(&cfg, &carried), Vec::<String>::new());
        // And it really does replace the steps, so it is usable where the footgun was written.
        let fewer = cfg.with_steps(Vec::new());
        assert_eq!(dag_config_carry_diff(&cfg, &fewer), vec!["steps: g.j -> "]);
    }

    #[test]
    fn an_absent_default_cap_is_reported_as_absent_not_as_zero() {
        let absent = DagConfig {
            default_step_mem_cap_bytes: None,
            ..Default::default()
        };
        let zero = DagConfig {
            default_step_mem_cap_bytes: Some(0),
            ..Default::default()
        };
        // ABSENT IS NOT ZERO: "disable the cap" and "cap at 0" are opposite instructions, so the
        // report must not collapse them into the same line.
        assert_eq!(
            dag_config_carry_diff(&absent, &zero),
            vec!["default_step_mem_cap_bytes: <absent> -> 0"]
        );
    }

    #[test]
    fn a_nan_factor_is_not_reported_as_a_dropped_field() {
        // NaN != NaN, so a naive float comparison would report an untouched config as changed --
        // an assertion that fires on a config nobody rebuilt is one nobody keeps.
        let nan = DagConfig {
            mem_cap_factor: f64::NAN,
            ..Default::default()
        };
        assert_eq!(dag_config_carry_diff(&nan, &nan), Vec::<String>::new());
    }

    #[test]
    fn the_field_checklist_matches_the_comparison() {
        // DAG_CONFIG_FIELDS is prose until something checks it. The compiler already forces a new
        // DagConfig field into `dag_config_carry_diff` (exhaustive destructuring, no `..`); this
        // pins the count so the written-down list cannot drift away from what is compared.
        let cfg = configured();
        let diff = dag_config_carry_diff(&cfg, &DagConfig::default());
        assert_eq!(
            diff.len(),
            DAG_CONFIG_FIELDS.len(),
            "every field differs between a fully-configured DAG and the defaults: {diff:?}"
        );
    }
}
