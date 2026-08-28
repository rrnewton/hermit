// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Plan construction for the validate driver: turn a PROFILE into a `DagConfig`.
//!
//! # The single rule this module exists to enforce
//!
//! **Nothing validate runs may execute outside `dagrun`.** Every gate
//! — preflight submodule init, the Reverie pin check, the manifest gate, each CI
//! lane node, and each compatibility probe — is a DAG *node*. The driver makes
//! exactly one kind of call (`run_dag_boxed_ordered`) and never spawns work
//! itself. The previous Phase-1 wrapper had a `run_subprocess_gate` helper that
//! shelled out for the three preflight gates; that was a second execution path
//! inside the driver, so those gates were unboxed, untimed by the runner, and
//! invisible to its typed accounting. It is gone.
//!
//! # Every synthesized node MUST declare its caps — measured, not assumed
//!
//! `dagrun` applies its SMALL "forcing function" floor (1 GiB / 1 core
//! / 10 s CPU) **only** through its own CLI, behind `--small-default-cap`. A
//! LIBRARY consumer — which this driver is — gets `DagConfig::default()`, i.e.
//! `default_step_mem_cap_bytes: None`, `default_step_cpu_count: None`,
//! `default_step_cpu_timeout: 0`. That is deliberate on the runner's side (an
//! always-on floor would wedge concurrent validates on the shared checkout), but
//! it means **an undeclared node is boxed in name only**.
//!
//! Measured on this box at the time of writing, through this exact library path:
//! a node declaring nothing allocated 2 GiB and burned 40 s of CPU and PASSED.
//! A node declaring `hard_mem_max_bytes = 256 MiB` and allocating 4 GiB was
//! `OOM-KILLED (hit inner MemoryMax; 3 oom_kill event(s))` at `peak≈256.0 MiB`
//! and failed the run. Boxing works; it just has to be asked for.
//!
//! So: every node built here declares `timeout`, `cpu_timeout`, and a memory
//! hint, and [`undeclared_nodes`] is the fail-closed audit that keeps it true.
//!
//! Note also that every shipped lane node declares a memory hint, while the
//! older portable and privileged nodes still inherit their CPU-time guard from
//! this module's profile-level `default_step_cpu_timeout`. The direct
//! compatibility probes declare their shorter CPU bound individually.

use std::collections::BTreeMap;
use std::path::Path;

use dagrun::io::dag_from_json;
use dagrun::model::DagConfig;
use dagrun::model::ResourceHint;
use dagrun::model::Step;
use dagrun::model::StepClass;

use crate::validate_corpus;
use crate::validate_corpus::CorpusPaths;

pub const E2E_ATTEMPT_ENV: &str = "E2E_ATTEMPT";

/// Wall budget for the preflight gates. Submodule init reaches the network
/// through `with-proxy`, so it needs more than a trivial ceiling but must not
/// inherit a lane-sized one.
const PREFLIGHT_TIMEOUT_S: i64 = 900;
/// CPU budget for preflight. These gates are I/O-bound (clone, fetch, a small
/// rustc); a tight CPU ceiling catches a spin without flaking under host load.
const PREFLIGHT_CPU_TIMEOUT_S: i64 = 300;
/// Memory ceiling for a preflight gate. `git submodule update --recursive` on
/// this tree peaks well under a GiB; 2 GiB leaves headroom without being a
/// non-cap.
const PREFLIGHT_MEM_BYTES: i64 = 2 * 1024 * 1024 * 1024;

/// The manifest audit is an executable consumer, so its producer is part of
/// the always-on preflight spine rather than an incidental lane root.
pub const MANIFEST_PLAN_PRODUCER_TAG: &str = "setup.manifest_plan";
pub const MANIFEST_PLAN_BUILD_COMMAND: &str = "cargo build -p hermit-manifest-plan --bins";
const MANIFEST_PLAN_BUILD_TIMEOUT_S: i64 = 180;
const MANIFEST_PLAN_BUILD_MEM_BYTES: i64 = 2 * 1024 * 1024 * 1024;

/// Per-lane-node CPU budget applied as the DAG-level default for nodes that do
/// not declare one. Generous relative to the wall timeout because
/// the build spine legitimately burns many CPU-minutes; it exists to stop an
/// unbounded spin, not to police normal cost.
const LANE_DEFAULT_CPU_TIMEOUT_S: i64 = 7200;

/// Wall budget for one compatibility probe. Mirrors `STRICT_COMPAT_TIMEOUT=60`
/// (validate.sh:1091).
const COMPAT_TIMEOUT_S: i64 = 60;
/// Shortened budget for a bounded portable diagnostic row (validate.sh:2969).
const COMPAT_PORTABLE_DIAGNOSTIC_TIMEOUT_S: i64 = 20;
/// Extended budget for the two large internal executables under e9patch
/// (validate.sh:2991).
const COMPAT_E9PATCH_LARGE_TIMEOUT_S: i64 = 180;
/// CPU budget for a compatibility probe: these are short guest runs under Hermit,
/// so a spin is the failure mode a CPU cap catches.
const COMPAT_CPU_TIMEOUT_S: i64 = 120;
/// Memory ceiling for a compatibility probe.
const COMPAT_MEM_BYTES: i64 = 4 * 1024 * 1024 * 1024;

/// Portable strict compatibility used to run as one outer node containing a
/// second 16-wide scheduler.  The flattened graph keeps that same maximum
/// overlap while letting the outer scheduler see every probe.
pub const PORTABLE_STRICT_COMPAT_CONCURRENCY: i64 = 16;
pub const PORTABLE_STRICT_COMPAT_RESOURCE: &str = "hermit_guest";
pub const PORTABLE_STRICT_COMPAT_FIXTURES: &str = "target/real-compat-fixtures";
pub const PORTABLE_STRICT_COMPAT_TMP: &str = "target/validation/strict-compat";
pub const PORTABLE_STRICT_COMPAT_SHELL_BUILD: &str =
    "target/validation/strict-compat/shell-build";
pub const PORTABLE_STRICT_COMPAT_HERMIT: &str = "target/ci/hermit-strict";

/// Which compatibility corpus a focused mode runs, and how it is labelled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompatMode {
    Strict,
    PortableStrict,
    Sabre,
    E9patch,
    Rr,
}

impl CompatMode {
    /// The `ci/compat/corpus-<mode>.json` file this mode reads. `PortableStrict`
    /// shares `strict`'s corpus: `PORTABLE_STRICT_PROBE_ARGS` changes the Hermit
    /// FLAGS, never corpus membership (validate.sh:2965).
    pub fn corpus_name(self) -> &'static str {
        match self {
            CompatMode::Strict | CompatMode::PortableStrict => "strict",
            CompatMode::Sabre => "sabre",
            CompatMode::E9patch => "e9patch",
            CompatMode::Rr => "rr",
        }
    }

    /// Plain-language name printed per row and in the summary.
    pub fn display_name(self) -> &'static str {
        match self {
            CompatMode::Strict | CompatMode::PortableStrict => "legacy below-L2 stripped verify",
            CompatMode::Sabre => "SaBRe legacy below-L2 stripped verify",
            CompatMode::E9patch => "e9patch legacy below-L2 stripped verify",
            CompatMode::Rr => "rr",
        }
    }

    /// The `hermit run ...` flags preceding `--`, reproducing the `run_args`
    /// selection in `strict_compatibility_probe` (validate.sh:2964-2994).
    pub fn run_args(self, label: &str, nsswitch: &str) -> Vec<String> {
        let s = |v: &str| v.to_string();
        match self {
            CompatMode::Strict => vec![
                s("run"),
                s("--strict"),
                s("--verify"),
                s("--env"),
                s("TMPDIR=/tmp"),
                s("--"),
            ],
            CompatMode::PortableStrict => vec![
                s("run"),
                s("--strict"),
                s("--verify"),
                s("--no-virtualize-cpuid"),
                s("--max-timeslice=disabled"),
                s("--env"),
                s("TMPDIR=/tmp"),
                s("--"),
            ],
            CompatMode::Sabre => {
                vec![s("run"), s("--backend"), s("sabre"), s("--strict"), s("--verify"), s("--")]
            }
            CompatMode::E9patch => {
                let mut v = vec![s("run"), s("--backend"), s("e9patch")];
                // These rows query owner names the host may delegate to an async
                // identity daemon; pin just them to the files-only NSS fixture
                // (validate.sh:2981).
                if matches!(label, "whoami" | "groups" | "pinky" | "logname" | "tar" | "chown") {
                    v.push(format!(
                        "--mount=type=bind,source={nsswitch},target=/etc/nsswitch.conf,readonly"
                    ));
                }
                v.push(s("--strict"));
                v.push(s("--verify"));
                v.push(s("--"));
                v
            }
            // rr rows are driven through `hermit record start --verify`, matching
            // rr_compatibility_probe rather than the plain run path.
            CompatMode::Rr => {
                vec![s("record"), s("start"), s("--verify"), s("--verify-strict"), s("--")]
            }
        }
    }

    /// Per-row wall budget, reproducing the two budget overrides the bash applies.
    pub fn timeout_for(self, label: &str) -> i64 {
        if self == CompatMode::PortableStrict
            && validate_corpus::portable_diagnostic().contains_key(label)
        {
            return COMPAT_PORTABLE_DIAGNOSTIC_TIMEOUT_S;
        }
        if self == CompatMode::E9patch && matches!(label, "mysql" | "php") {
            return COMPAT_E9PATCH_LARGE_TIMEOUT_S;
        }
        COMPAT_TIMEOUT_S
    }
}

/// What the compatibility summary should DO about one measured row.
///
/// Extracted as a pure function of (mode, outcome, table membership) so the decision can be
/// bracketed without running a guest, and so the reporting text and the blocking verdict cannot
/// drift apart -- they are now two readings of the same value rather than two independent
/// branches of one `if`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatDisposition {
    /// Passed, and nothing in the tables says otherwise.
    Passed,
    /// Passed while listed as known fail-closed. The EXPECTATION is stale, not the run.
    PassedButListedFailClosed,
    /// Failed, listed, and exempted. This is `Strict`'s historical behaviour and is deliberately
    /// confined to it.
    KnownFailClosedExempt,
    /// Failed while listed as known fail-closed, and STILL BLOCKING. Reporting the row's reason
    /// is not the same as excusing it; this variant exists so the reason can be printed without
    /// the failure being downgraded.
    KnownFailClosedBlocking,
    /// Failed as a bounded portable diagnostic: nonblocking by prior policy.
    PortableDiagnostic,
    /// Failed with nothing to say about it.
    Blocking,
}

impl CompatDisposition {
    /// Whether this row must fail the run.
    ///
    /// The ONE property that must not regress while reporting improves. `KnownFailClosedBlocking`
    /// is deliberately blocking: a listed row under `PortableStrict` was already blocking before
    /// the reason was printed, and printing it must not change that.
    pub fn is_blocking(self) -> bool {
        matches!(self, CompatDisposition::KnownFailClosedBlocking | CompatDisposition::Blocking)
    }
}

/// Classify one measured compatibility row.
///
/// Pure: it takes membership as booleans rather than the tables themselves, so a bracket can
/// exercise every combination without constructing or planting a corpus, and so production
/// keeps reading the real tables.
///
/// `Strict` keeps its exemption. `PortableStrict` gains REPORTING ONLY. Every other mode --
/// `Sabre`, `E9patch`, `Rr` -- consults neither table and gains nothing: a failure there is
/// blocking exactly as before.
pub fn classify_compat_outcome(
    mode: CompatMode,
    ok: bool,
    listed_failclosed: bool,
    listed_diagnostic: bool,
) -> CompatDisposition {
    // Only the two strict modes consult the fail-closed table at all; it describes what
    // `--strict` refuses, which says nothing about the other backends.
    let consults_failclosed =
        matches!(mode, CompatMode::Strict | CompatMode::PortableStrict) && listed_failclosed;
    if ok {
        if consults_failclosed {
            return CompatDisposition::PassedButListedFailClosed;
        }
        return CompatDisposition::Passed;
    }
    if consults_failclosed {
        return match mode {
            CompatMode::Strict => CompatDisposition::KnownFailClosedExempt,
            _ => CompatDisposition::KnownFailClosedBlocking,
        };
    }
    if mode == CompatMode::PortableStrict && listed_diagnostic {
        return CompatDisposition::PortableDiagnostic;
    }
    CompatDisposition::Blocking
}

/// Build a fully-declared node. This is the ONLY node constructor the plan
/// modules use, so a node cannot be created without caps. It is `pub(crate)` in
/// spirit — `validate_super` and `validate_envelope` call it precisely so that
/// their nodes cannot skip the cap declaration either.
pub fn node(
    group: &str,
    job: &str,
    desc: &str,
    cmd: String,
    deps: Vec<String>,
    timeout: i64,
    cpu_timeout: i64,
    mem_bytes: i64,
) -> Step {
    Step {
        group: group.to_string(),
        job: job.to_string(),
        desc: desc.to_string(),
        description: String::new(),
        cmd,
        deps,
        env: BTreeMap::new(),
        hint: ResourceHint {
            rss_baseline_bytes: Some(mem_bytes),
            hard_mem_max_bytes: Some(mem_bytes),
            ..Default::default()
        },
        networkonly: false,
        engine_only: false,
        timeout,
        cpu_timeout,
        jobs_flag: None,
        jobs_env: None,
        skip_reason: None,
        // `None` means "this step declares nothing", which is what every node here
        // meant before the runner grew these fields. `Some(vec![])` would be the
        // stronger claim that the step writes to none of the policy's protected
        // domains, and nothing in this plan has established that. Hermit's DAGs set
        // no write-domain policy, so `require_explicit` is false and an omitted
        // declaration is accepted rather than silently treated as a guarantee.
        write_domains: None,
        write_domain_guarantee: None,
        explains: Vec::new(),
        fail_fast_family: None,
    }
}

/// Shell-quote one argv element for embedding in a `bash -c` command string.
///
/// The corpus carries argv ARRAYS (that is how it was extracted, and it is what
/// keeps a workload containing spaces, quotes, or `$` from being re-split). The
/// runner takes a single shell string, so each element is single-quoted here with
/// the standard `'\''` escape. Getting this wrong would silently mutate guest
/// commands, so it is exercised by `--self-test`.
pub fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Join an argv into a shell command string.
pub fn shell_join<I: IntoIterator<Item = S>, S: AsRef<str>>(argv: I) -> String {
    argv.into_iter()
        .map(|a| shell_quote(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The always-on preflight gates and the manifest-audit binary producer, as DAG nodes.
///
/// Submodule verification is deliberately non-mutating and first. Initializing
/// or repairing a checkout before observing it would erase the exact drift this
/// gate exists to detect. A caller with an uninitialized checkout must run
/// `make checkout-all` explicitly, then retry validation.
pub fn preflight_nodes(root: &Path, with_proxy: bool) -> Vec<Step> {
    let proxy = if with_proxy { "with-proxy " } else { "" };
    // The Reverie-pin launcher is bound to THIS repository explicitly, never left
    // to whatever directory the node happens to start in. `target/debug/test-harness`'s
    // `assert_reverie_pin_enforcement` audits that binding, because "it will be
    // the right repo because cwd is right" is an inference, not an observation —
    // and the archival pin is not a testing exemption.
    let root = shell_quote(&root.to_string_lossy());
    let mut manifest_plan = node(
        "setup",
        "manifest_plan",
        "Build the manifest-plan binaries the metadata validation runs",
        MANIFEST_PLAN_BUILD_COMMAND.to_string(),
        vec!["pre.reverie_pin".to_string()],
        MANIFEST_PLAN_BUILD_TIMEOUT_S,
        LANE_DEFAULT_CPU_TIMEOUT_S,
        MANIFEST_PLAN_BUILD_MEM_BYTES,
    );
    manifest_plan.hint.est_duration_s = 60.0;
    manifest_plan.hint.classification = StepClass::CpuBound;

    vec![
        node(
            "pre",
            "submodules",
            "Verify repository submodules without initializing or repairing them",
            "./ci/verify-submodules.sh --self-test && ./ci/verify-submodules.sh".to_string(),
            vec![],
            PREFLIGHT_TIMEOUT_S,
            PREFLIGHT_CPU_TIMEOUT_S,
            PREFLIGHT_MEM_BYTES,
        ),
        node(
            // Tag must stay `pre.reverie_pin`: scripts/validate.rs asserts a
            // passing node with exactly this tag before it will emit a PASS.
            "pre",
            "reverie_pin",
            "Reverie pin consistency",
            format!("{proxy}{root}/ci/run-reverie-pin-check.sh --repo {root}"),
            vec!["pre.submodules".to_string()],
            PREFLIGHT_TIMEOUT_S,
            PREFLIGHT_CPU_TIMEOUT_S,
            PREFLIGHT_MEM_BYTES,
        ),
        manifest_plan,
        node(
            "gate",
            "manifest",
            "Centralized test manifest and inventory",
            "target/debug/test-harness validate".to_string(),
            vec![MANIFEST_PLAN_PRODUCER_TAG.to_string()],
            PREFLIGHT_TIMEOUT_S,
            PREFLIGHT_CPU_TIMEOUT_S,
            PREFLIGHT_MEM_BYTES,
        ),
    ]
}

/// Remove a lane's duplicate manifest-plan producer and bind its consumers to
/// the always-on preflight producer.
///
/// Lane JSON retains the producer because it is also executable independently.
/// Once a lane is attached to validate's preflight, however, keeping that root
/// would either duplicate the tag or recreate the old `gate -> producer -> gate`
/// cycle. Refuse command drift before remapping the dependency.
pub fn lane_nodes_reusing_manifest_producer(
    root: &Path,
    lane: &str,
    prefix: &str,
    gate_dep: &str,
) -> Result<Vec<Step>, String> {
    let mut steps = lane_nodes(root, lane, prefix, gate_dep)?;
    if !reuse_preflight_manifest_producer(&mut steps, &format!("lane {lane}"))? {
        return Err(format!("lane {lane} has no manifest-plan producer"));
    }
    Ok(steps)
}

/// Replace a selected lane's manifest-plan producer with the equivalent node
/// already present in validate's preflight spine.
///
/// Returning `false` for an absent producer is intentional: a selective set may
/// not need any manifest-plan consumer. If the selector did include it, its tag
/// remains valid selection vocabulary and every selected dependent is remapped
/// to the preflight node before the lane is attached.
pub fn reuse_preflight_manifest_producer(
    steps: &mut Vec<Step>,
    context: &str,
) -> Result<bool, String> {
    let producer_indexes: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter_map(|(index, step)| (step.job == "manifest_plan").then_some(index))
        .collect();
    if producer_indexes.len() > 1 {
        return Err(format!(
            "{context} contains {} manifest-plan producers",
            producer_indexes.len()
        ));
    }
    let Some(index) = producer_indexes.first().copied() else {
        return Ok(false);
    };
    if steps[index].cmd != MANIFEST_PLAN_BUILD_COMMAND {
        return Err(format!(
            "{context} manifest-plan producer command drifted: {}",
            steps[index].cmd
        ));
    }
    let lane_producer_tag = steps[index].tag();
    steps.remove(index);
    for step in steps.iter_mut() {
        for dependency in &mut step.deps {
            if dependency == &lane_producer_tag {
                *dependency = MANIFEST_PLAN_PRODUCER_TAG.to_string();
            }
        }
        step.deps.sort();
        step.deps.dedup();
    }
    Ok(true)
}

/// THE one place a CI lane's file is resolved.
///
/// `target/debug/test-harness` audits that this expression appears EXACTLY ONCE in this
/// file, so that a lane's node set can never be resolved from two places that
/// could drift. Both `lane_nodes` (steps) and `lane_config` (top-level config)
/// go through here; adding a second construction of the path is what the audit
/// exists to catch, and it caught exactly that when `lane_config` was added.
pub fn lane_dag_path(root: &Path, lane: &str) -> std::path::PathBuf {
    root.join("ci").join("dag").join(format!("{lane}.json"))
}

/// Load one shipped CI lane (`ci/dag/<lane>.json`) and hang it off the preflight.
///
/// `prefix` disambiguates tags when two lanes are fused into one DAG; it is empty
/// for a single-lane run so tags stay byte-identical to the shipped file (which
/// keeps `ci/run-node.sh`, the perf store, and the coverage predicate keyed the
/// same way).
pub fn lane_nodes(
    root: &Path,
    lane: &str,
    prefix: &str,
    gate_dep: &str,
) -> Result<Vec<Step>, String> {
    let path = lane_dag_path(root, lane);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let cfg = dag_from_json(&text).map_err(|e| format!("invalid DAG {}: {e}", path.display()))?;
    let portable_compat_commands = if lane == "portable" {
        Some(
            portable_strict_compat_probe_nodes(root)?
                .into_iter()
                .map(|step| (step.tag(), step.cmd))
                .collect::<BTreeMap<_, _>>(),
        )
    } else {
        None
    };
    let retag = |g: &str| if prefix.is_empty() { g.to_string() } else { format!("{prefix}{g}") };
    let mut seen_portable_compat = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(cfg.steps.len());
    for s in &cfg.steps {
        let mut step = s.clone();
        if let Some(commands) = &portable_compat_commands {
            if s.group == "compat" {
                let command = commands.get(&s.tag()).ok_or_else(|| {
                    format!(
                        "portable DAG contains compatibility node {} absent from ci/compat/corpus-strict.json",
                        s.tag()
                    )
                })?;
                let stored = portable_strict_compat_stored_command(&s.job, command);
                if s.cmd != stored {
                    return Err(format!(
                        "portable strict compatibility command drifted for {}: expected {:?}, got {:?}",
                        s.tag(), stored, s.cmd
                    ));
                }
                // The standalone lane treats the three declared portable
                // diagnostics as warnings, matching the former focused
                // validator.  This validator consumes typed outcomes itself,
                // so run the underlying command and let the compatibility
                // disposition decide whether its failure blocks.
                step.cmd = command.clone();
                seen_portable_compat.insert(s.tag());
            }
        }
        // All buckets in one validate share E2E_RUN_ID, so the harness defaults
        // would make concurrent processes append unrelated bucket rows to one
        // file. Keep one run identity while isolating storage by lane and bucket.
        if s.group == "e2e" && s.job.starts_with("manifest_") {
            step.cmd.push_str(&format!(
                " --results \"$E2E_RESULT_ROOT/{lane}/{}/results.jsonl\" --junit \"$E2E_RESULT_ROOT/{lane}/{}/junit.xml\"",
                s.job, s.job
            ));
            step.env.insert(E2E_ATTEMPT_ENV.into(), "1".into());
        }
        step.group = retag(&s.group);
        step.deps = s
            .deps
            .iter()
            .map(|d| match d.split_once('.') {
                Some((g, j)) => format!("{}.{}", retag(g), j),
                None => d.clone(),
            })
            .collect();
        // Every lane node waits on the manifest gate, reproducing
        // run_ci_manifest_lane's ordering (validate.sh:4344).
        if step.deps.is_empty() {
            step.deps.push(gate_dep.to_string());
        }
        // Supply a memory cap for any lane node that shipped without one, so the
        // "declared caps" audit below cannot be satisfied by an unboxed node.
        if step.hint.rss_baseline_bytes.is_none() && step.hint.hard_mem_max_bytes.is_none() {
            step.hint.hard_mem_max_bytes = Some(8 * 1024 * 1024 * 1024);
        }
        out.push(step);
    }
    if let Some(commands) = &portable_compat_commands {
        let missing = commands
            .keys()
            .filter(|tag| !seen_portable_compat.contains(*tag))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(format!(
                "portable DAG is missing {} compatibility node(s) derived from ci/compat/corpus-strict.json: {}",
                missing.len(),
                missing.join(", ")
            ));
        }
    }
    Ok(out)
}

// ------------------------------------------------- host-capability requirements
//
// A node can require a facility the MACHINE either has or does not have. Before
// this section such a node had exactly two outcomes — it passed, or it failed —
// so "this host cannot run it" and "this ran and it is broken" were the same
// record. On a host without CPUID faulting `privileged-cpuid.faulting` failed in
// 0.11 s with exit 101 and an empty detail block, which reads like a broken
// build, and its eager-exit cost the other twelve in-flight nodes
// (hermit#2135, hermit#2148, hermit#2205).
//
// The fix is a THIRD recorded outcome, host-inapplicable, decided BEFORE the
// node is spawned. Five properties keep it from becoming a way to excuse a node
// that is merely broken:
//
//  1. The decision never reads the node. It is made from an out-of-band probe of
//     the machine during plan construction; a node's exit code, stderr, or panic
//     message cannot produce it. A broken node still runs, still fails, and is
//     still refused.
//  2. The capability vocabulary is CLOSED and lives in this file
//     ([`HostCapability`]). A DAG naming an unknown capability is a
//     plan-construction refusal, not a skip.
//  3. The probe fails closed TOWARD RUNNING. Absence requires two independent
//     sources to agree; a probe error, an unexpected errno, an unreadable
//     `/proc/cpuinfo`, or disagreement between the sources all resolve to
//     PRESENT, so the node runs and any failure is real.
//  4. There is no override that manufactures absence.
//     `HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT` can only force a capability
//     PRESENT, i.e. can only cause MORE to run.
//  5. Recording it honestly is what costs the receipt. The node is written to
//     the ledger as a typed intentional skip whose reason is `host-inapplicable`,
//     and the parent's separately-reviewed consumer allowlist
//     (`ci-hub/validate/gate_completeness.py::ALLOWED_INTENTIONAL_SKIP_REASONS`,
//     `ci-hub/lib/qualifying_receipt.rs::intentional_skip_count`) admits only
//     `empty-manifest-bucket`. A run carrying a host-inapplicable node therefore
//     does NOT qualify as landing authority until the owner opts that reason in.
//     The mechanism cannot buy a green; it can only stop one node's absence from
//     destroying the other forty.

/// A machine facility a node needs before it can run at all.
///
/// CLOSED VOCABULARY. `ci/dag/<lane>.json` may name one of these on a step
/// through `requires_host_capability`, but only a name listed here parses;
/// anything else is a refusal. Adding a member is a reviewed source change in
/// this file, so editing a DAG alone can never invent a new reason for a node
/// not to run.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum HostCapability {
    /// CPUID faulting: `arch_prctl(ARCH_SET_CPUID, 0)` succeeds, so the kernel
    /// can trap the guest's `CPUID` and Detcore can mask host feature bits.
    /// Advertised by the kernel as the `cpuid_fault` flag in `/proc/cpuinfo`.
    CpuidFaulting,
    /// Hardware virtualization reachable through `/dev/kvm`, so the KVM backend
    /// can create a VM. Corroborated by the `vmx`/`svm` flags in `/proc/cpuinfo`.
    Kvm,
}

impl HostCapability {
    /// The stable serialized name used in `ci/dag/<lane>.json` and the ledger.
    pub fn value(self) -> &'static str {
        match self {
            Self::CpuidFaulting => "cpuid-faulting",
            Self::Kvm => "kvm",
        }
    }

    /// Parse one stable serialized name; `None` for anything unrecognized.
    pub fn from_value(text: &str) -> Option<Self> {
        match text {
            "cpuid-faulting" => Some(Self::CpuidFaulting),
            "kvm" => Some(Self::Kvm),
            _ => None,
        }
    }
}

/// The stable ledger reason for a node the machine provably cannot run.
///
/// The word is the owner's, from hermit#2205: "The missing concept is a third,
/// recorded outcome: **host-inapplicable**, distinct from both pass and fail."
pub const HOST_INAPPLICABLE_REASON: &str = "host-inapplicable";

/// The one environment override, deliberately one-directional: it can only add
/// capabilities to the PRESENT set. Nothing can force a capability ABSENT.
pub const ASSUME_PRESENT_ENV: &str = "HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT";

/// One capability verdict for THIS machine, with the observation behind it.
#[derive(Clone, Debug)]
pub struct CapabilityVerdict {
    pub capability: HostCapability,
    /// `true` unless absence was positively established. Doubt means present.
    pub present: bool,
    /// What was actually observed, recorded verbatim so a reader never has to
    /// take "inapplicable" on trust.
    pub evidence: String,
}

/// One node the machine provably cannot run, and why.
#[derive(Clone, Debug)]
pub struct HostInapplicableNode {
    pub tag: String,
    pub capability: HostCapability,
    pub evidence: String,
}

/// The `requires_host_capability` declarations in one lane, keyed by runner tag.
///
/// Reads the same file `lane_nodes` reads, through the same single path helper.
/// `prefix` matches the retagging `lane_nodes` applies when lanes are fused.
/// An unparseable capability name is an ERROR: refusing the whole run is the
/// only safe response to a declaration nobody can evaluate.
pub fn lane_host_capability_requirements(
    root: &Path,
    lane: &str,
    prefix: &str,
) -> Result<BTreeMap<String, HostCapability>, String> {
    let path = lane_dag_path(root, lane);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let raw: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("invalid DAG {}: {e}", path.display()))?;
    let steps = raw
        .get("steps")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("invalid DAG {}: 'steps' must be a list", path.display()))?;
    let mut out = BTreeMap::new();
    for step in steps {
        let Some(declared) = step.get("requires_host_capability") else {
            continue;
        };
        let name = declared.as_str().ok_or_else(|| {
            format!(
                "{}: requires_host_capability must be a string",
                path.display()
            )
        })?;
        let capability = HostCapability::from_value(name).ok_or_else(|| {
            format!(
                "{}: unknown requires_host_capability '{name}'; the capability vocabulary is \
                 closed (scripts/lib/validate_plan.rs::HostCapability) and an unrecognized name \
                 is refused rather than treated as a reason to omit a node",
                path.display()
            )
        })?;
        let group = step.get("group").and_then(serde_json::Value::as_str).unwrap_or_default();
        let job = step.get("job").and_then(serde_json::Value::as_str).unwrap_or_default();
        if group.is_empty() || job.is_empty() {
            return Err(format!(
                "{}: a step declaring requires_host_capability has no group.job identity",
                path.display()
            ));
        }
        out.insert(format!("{prefix}{group}.{job}"), capability);
    }
    Ok(out)
}

/// Every `requires_host_capability` declaration in every shipped lane, under
/// both the bare and the fused-lane tag spelling, so the caller can look a plan's
/// tags up directly however the plan was assembled.
pub fn host_capability_requirements(
    root: &Path,
) -> Result<BTreeMap<String, HostCapability>, String> {
    let mut out = BTreeMap::new();
    for lane in ["portable", "privileged"] {
        for prefix in ["".to_string(), format!("{lane}-")] {
            out.extend(lane_host_capability_requirements(root, lane, &prefix)?);
        }
    }
    Ok(out)
}

/// Ask the MACHINE whether it has one capability.
///
/// This is PLAN CONSTRUCTION, in the same class as `ask_selector`: it
/// produces no verdict about the tree and runs no gate. Every failure mode
/// resolves to PRESENT, so the driver can only ever err toward running MORE.
pub fn probe_host_capability(capability: HostCapability) -> CapabilityVerdict {
    let forced = std::env::var(ASSUME_PRESENT_ENV).unwrap_or_default();
    if forced.split(',').map(str::trim).any(|c| c == capability.value()) {
        return CapabilityVerdict {
            capability,
            present: true,
            evidence: format!("{ASSUME_PRESENT_ENV} names {}; assumed PRESENT without probing (this override can only ADD capabilities)", capability.value()),
        };
    }
    match capability {
        HostCapability::CpuidFaulting => probe_cpuid_faulting(),
        HostCapability::Kvm => probe_kvm(),
    }
}

/// `arch_prctl(ARCH_SET_CPUID, 0)` in a forked child, exactly as
/// `detcore/tests/misc/mod.rs::cpuid_faulting_supported` probes it.
///
/// Returns `Ok(())` when the kernel accepted it, or `Err(errno)`. `Err(0)` means
/// the child died without reporting an errno, which is doubt, not absence.
fn arch_prctl_set_cpuid_off() -> Result<(), i32> {
    const ARCH_SET_CPUID: libc::c_int = 0x1012;
    // SAFETY: the child performs one syscall and `_exit`s; it never returns into
    // Rust, allocates, or touches inherited locks.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(0);
    }
    if child == 0 {
        let result = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_CPUID, 0) };
        let code = if result == 0 {
            0
        } else {
            let errno = unsafe { *libc::__errno_location() };
            errno.clamp(1, 255)
        };
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    if unsafe { libc::waitpid(child, &mut status, 0) } != child || !libc::WIFEXITED(status) {
        return Err(0);
    }
    match libc::WEXITSTATUS(status) {
        0 => Ok(()),
        errno => Err(errno),
    }
}

/// Does `/proc/cpuinfo` advertise the kernel's `cpuid_fault` flag?
///
/// `None` when `/proc/cpuinfo` could not be read — unknown, never "absent".
fn cpuinfo_advertises_cpuid_fault() -> Option<bool> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    Some(text.split_whitespace().any(|word| word == "cpuid_fault"))
}

/// TWO INDEPENDENT SOURCES must agree before this capability is called absent:
/// the kernel must refuse `arch_prctl(ARCH_SET_CPUID, 0)` with `ENODEV` AND
/// `/proc/cpuinfo` must not advertise `cpuid_fault`. Any other combination —
/// success, a different errno, a fork failure, an unreadable `/proc/cpuinfo`, or
/// the two sources disagreeing — is PRESENT, so the node runs and a real
/// regression still turns the run red.
///
/// PURE, so `--self-test` can bracket the conjunction with planted observations
/// on a machine of either kind. `ENODEV` specifically, not "any failure":
/// `EPERM` from a restricted sandbox or `EINVAL` from an unexpected kernel is
/// doubt about the PROBE, and doubt runs the node.
pub fn cpuid_faulting_absent(syscall: Result<(), i32>, advertised: Option<bool>) -> bool {
    syscall == Err(libc::ENODEV) && advertised == Some(false)
}

fn probe_cpuid_faulting() -> CapabilityVerdict {
    let syscall = arch_prctl_set_cpuid_off();
    let advertised = cpuinfo_advertises_cpuid_fault();
    let syscall_text = match syscall {
        Ok(()) => "arch_prctl(ARCH_SET_CPUID, 0) = 0".to_string(),
        Err(0) => "arch_prctl(ARCH_SET_CPUID, 0) probe could not be completed".to_string(),
        Err(errno) => format!("arch_prctl(ARCH_SET_CPUID, 0) = -1 errno={errno}"),
    };
    let cpuinfo_text = match advertised {
        Some(true) => "/proc/cpuinfo advertises cpuid_fault",
        Some(false) => "/proc/cpuinfo does not advertise cpuid_fault",
        None => "/proc/cpuinfo could not be read",
    };
    CapabilityVerdict {
        capability: HostCapability::CpuidFaulting,
        present: !cpuid_faulting_absent(syscall, advertised),
        evidence: format!("{syscall_text}; {cpuinfo_text}"),
    }
}

/// Can this machine open `/dev/kvm` for reading and writing?
///
/// Returns `Ok(())` when the device accepted the open, or `Err(errno)`.
fn open_dev_kvm() -> Result<(), i32> {
    let path = std::ffi::CString::new("/dev/kvm").expect("static path has no NUL");
    // SAFETY: `path` is a valid NUL-terminated C string that outlives the call,
    // and the descriptor is closed immediately on success.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd >= 0 {
        // SAFETY: `fd` was just returned by a successful `open`.
        unsafe { libc::close(fd) };
        return Ok(());
    }
    // SAFETY: reading the thread-local errno immediately after a failed call.
    let errno = unsafe { *libc::__errno_location() };
    Err(errno.clamp(1, 255))
}

/// Does `/proc/cpuinfo` advertise hardware virtualization (`vmx` on Intel,
/// `svm` on AMD)?
///
/// `None` when `/proc/cpuinfo` could not be read — unknown, never "absent".
fn cpuinfo_advertises_virtualization() -> Option<bool> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    Some(text
        .split_whitespace()
        .any(|word| word == "vmx" || word == "svm"))
}

/// TWO INDEPENDENT SOURCES must agree before KVM is called absent, mirroring
/// `cpuid_faulting_absent`: opening `/dev/kvm` must fail with `ENOENT` AND
/// `/proc/cpuinfo` must advertise neither `vmx` nor `svm`. Anything else —
/// a successful open, `EACCES` from a restricted sandbox, an unreadable
/// `/proc/cpuinfo`, or the two sources disagreeing — is PRESENT, so the node
/// runs.
///
/// ⚠️ "DOUBT RUNS THE NODE" IS ONLY SAFE HERE BECAUSE THE NODE COUNTS ITS TESTS.
/// For `cpuid-faulting` a wrongly-run node fails loudly and the run turns red.
/// The `run_kvm_` tests do NOT behave that way: every one of them self-guards on
/// `Path::new("/dev/kvm").exists()` and RETURNS EARLY, so a wrongly-run node
/// would report 22 silent passes — a phantom green, strictly worse than the
/// unscheduled state it replaces, because the gap would then hide behind green
/// ticks instead of being greppable in the DAG.
///
/// The node therefore carries TWO guards, and they catch different things.
/// Neither substitutes for the other:
///
/// * it OPENS `/dev/kvm` before running anything and fails if it cannot. This is
///   what catches a wrongly-PRESENT probe, because an early-returning test still
///   counts as run and passed — a test count cannot see a guard fire.
/// * it asserts 22 tests were SELECTED. This catches the opposite regression: a
///   rename or a filter change silently shrinking the set, which is exactly how
///   these 22 became unobserved in the first place.
///
/// Getting that split wrong is easy: the count looks like it proves the tests
/// did work, and it does not.
///
/// PURE, so `--self-test` can bracket the conjunction with planted observations
/// on a machine of either kind.
pub fn kvm_absent(open: Result<(), i32>, advertised: Option<bool>) -> bool {
    open == Err(libc::ENOENT) && advertised == Some(false)
}

fn probe_kvm() -> CapabilityVerdict {
    let open = open_dev_kvm();
    let advertised = cpuinfo_advertises_virtualization();
    let open_text = match open {
        Ok(()) => "open(/dev/kvm, O_RDWR) = ok".to_string(),
        Err(errno) => format!("open(/dev/kvm, O_RDWR) = -1 errno={errno}"),
    };
    let cpuinfo_text = match advertised {
        Some(true) => "/proc/cpuinfo advertises vmx or svm",
        Some(false) => "/proc/cpuinfo advertises neither vmx nor svm",
        None => "/proc/cpuinfo could not be read",
    };
    CapabilityVerdict {
        capability: HostCapability::Kvm,
        present: !kvm_absent(open, advertised),
        evidence: format!("{open_text}; {cpuinfo_text}"),
    }
}

/// Split a step list into what will run and what the machine cannot run.
///
/// PURE: it consumes an already-resolved absence map, so `--self-test` brackets
/// both directions without touching the machine.
///
/// A step is withheld ONLY when it declares a capability that is in `absent`.
/// A step with no declaration is never withheld, whatever is absent — that is
/// what stops this from excusing a node that is merely broken.
///
/// Withholding a node that another RETAINED node depends on would silently
/// orphan work, so it is a refusal rather than a cascade.
pub fn partition_host_inapplicable(
    steps: Vec<Step>,
    requirements: &BTreeMap<String, HostCapability>,
    absent: &BTreeMap<HostCapability, String>,
) -> Result<(Vec<Step>, Vec<HostInapplicableNode>), String> {
    let mut keep = Vec::with_capacity(steps.len());
    let mut withheld = Vec::new();
    for step in steps {
        let tag = step.tag();
        match requirements.get(&tag) {
            Some(capability) if absent.contains_key(capability) => withheld.push(
                HostInapplicableNode {
                    tag,
                    capability: *capability,
                    evidence: absent[capability].clone(),
                },
            ),
            _ => keep.push(step),
        }
    }
    let gone: std::collections::BTreeSet<&str> =
        withheld.iter().map(|n| n.tag.as_str()).collect();
    let mut orphaned = Vec::new();
    for step in &keep {
        for dep in &step.deps {
            if gone.contains(dep.as_str()) {
                orphaned.push(format!("{} depends on {dep}", step.tag()));
            }
        }
    }
    if !orphaned.is_empty() {
        return Err(format!(
            "refusing to withhold a host-inapplicable node that other nodes depend on: {}; \
             a missing capability must not silently cascade into unrun work",
            orphaned.join(", ")
        ));
    }
    Ok((keep, withheld))
}

/// Build the compatibility-corpus nodes for one mode.
///
/// One DAG node PER PROBE. That is a deliberate change from the bash, which ran
/// all ~191 probes serially inside a single gate:
///   * each probe now gets its own wall + CPU + memory box, so one runaway row
///     cannot consume the whole gate's budget;
///   * each probe's verdict is a TYPED `StepOutcome`, so the summary table is
///     built from structured results instead of a scraped TSV; and
///   * the corpus becomes parallel, which is where a large part of the wall-clock
///     win in this profile is expected to come from.
pub fn compat_nodes(
    root: &Path,
    mode: CompatMode,
    hermit_bin: &str,
    nsswitch: &str,
    paths: &CorpusPaths,
    gate_dep: Option<&str>,
) -> Result<Vec<Step>, String> {
    compat_nodes_for(root, mode, hermit_bin, nsswitch, paths, gate_dep, None, None)
}

/// The portable strict probes as they appear in the shipped outer DAG.
///
/// Stable relative paths keep the committed DAG checkout-independent.  The
/// corpus remains the source of the argv; the self-test compares these generated
/// steps with every committed `compat.*` row so the reviewed list cannot drift.
pub fn portable_strict_compat_probe_nodes(root: &Path) -> Result<Vec<Step>, String> {
    let paths = CorpusPaths {
        root_dir: ".",
        real_compat_fixtures: PORTABLE_STRICT_COMPAT_FIXTURES,
        validation_tmp_dir: PORTABLE_STRICT_COMPAT_TMP,
        shell_build_dir: PORTABLE_STRICT_COMPAT_SHELL_BUILD,
    };
    compat_nodes(
        root,
        CompatMode::PortableStrict,
        PORTABLE_STRICT_COMPAT_HERMIT,
        "",
        &paths,
        Some("compatprep.fixtures"),
    )
}

/// Standalone `dagrun` consumers do not own the compatibility disposition
/// table, so the three already-declared portable diagnostics must preserve the
/// former focused validator's nonblocking result.  The validator replaces this
/// wrapper with the direct command above and judges the typed failure itself.
pub fn portable_strict_compat_stored_command(label: &str, command: &str) -> String {
    let diagnostics = validate_corpus::portable_diagnostic();
    let Some(reason) = diagnostics.get(label) else {
        return command.to_string();
    };
    format!(
        "status=0; {command} || status=$?; if [ \"$status\" -ne 0 ]; then printf 'WARN portable strict compatibility diagnostic %s failed (nonblocking), exit %s: %s\\n' {} \"$status\" {} >&2; fi; exit 0",
        shell_quote(label),
        shell_quote(reason),
    )
}

/// [`compat_nodes`] with two extra knobs used by the `super` suite's
/// `run_portable_slow_strict_diagnostics` port (validate.sh:4603).
///
/// * `only` restricts the corpus to an explicit label set AND suppresses the
///   `PORTABLE_STRICT_SUPER_ONLY` skip — because that gate exists precisely to
///   defer those four heavy rows *to this suite*, so the suite that runs them
///   must not also honor the deferral.
/// * `wall_override` replaces the per-row 60s corpus budget. The bash gave the
///   whole group of four one 600s `run_check_with_timeout`; each of these rows
///   is a full compile-link-run or JVM startup workload, so inheriting the
///   group's budget per node is the faithful reading. The 60s corpus default
///   would fail all four for lack of time and report it as a compatibility loss.
#[allow(clippy::too_many_arguments)]
pub fn compat_nodes_for(
    root: &Path,
    mode: CompatMode,
    hermit_bin: &str,
    nsswitch: &str,
    paths: &CorpusPaths,
    gate_dep: Option<&str>,
    only: Option<&std::collections::BTreeSet<String>>,
    wall_override: Option<i64>,
) -> Result<Vec<Step>, String> {
    let rows = validate_corpus::load(root, mode.corpus_name(), paths)?;
    let rr_allowed: Vec<&str> = validate_corpus::RR_PASSING_LABELS.to_vec();
    let super_only = validate_corpus::portable_super_only();
    let mut out = Vec::new();
    for row in rows {
        if let Some(keep) = only {
            if !keep.contains(&row.label) {
                continue;
            }
        }
        // rr measures ONLY the labels proven to pass record/replay; the bash
        // applies the same filter inside rr_compatibility_probe.
        if mode == CompatMode::Rr && !rr_allowed.contains(&row.label.as_str()) {
            continue;
        }
        // Heavy runtime workloads are deferred out of the portable profile to the
        // scheduled super suite (validate.sh:3090) — unless this IS that suite,
        // which names them explicitly through `only`.
        if only.is_none()
            && mode == CompatMode::PortableStrict
            && super_only.contains_key(row.label.as_str())
        {
            continue;
        }
        let mut argv: Vec<String> = vec![hermit_bin.to_string()];
        argv.extend(mode.run_args(&row.label, nsswitch));
        argv.extend(row.argv.iter().cloned());
        let wall = wall_override.unwrap_or_else(|| mode.timeout_for(&row.label));
        let mut step = node(
            "compat",
            &sanitize_job(&row.label),
            &format!("{} compatibility: {}", mode.display_name(), row.label),
            format!("{} </dev/null", shell_join(&argv)),
            gate_dep.map(|d| vec![d.to_string()]).unwrap_or_default(),
            wall,
            COMPAT_CPU_TIMEOUT_S.max(wall),
            COMPAT_MEM_BYTES,
        );
        if mode == CompatMode::PortableStrict {
            step.hint
                .resources
                .insert(PORTABLE_STRICT_COMPAT_RESOURCE.to_string(), 1);
        }
        out.push(step);
    }
    if out.is_empty() {
        return Err(format!("compatibility mode {mode:?} selected zero probes"));
    }
    Ok(out)
}

/// Keep only the named lane nodes, pruning each survivor's deps to the kept set.
///
/// Port of `build_selected_portable_dag` (validate.sh:4400), which did the same
/// `jq` surgery into a temporary DAG file consumed through
/// `RUN_DAG_FILE_OVERRIDE`. Here the plan is already in memory, so no temp file
/// and no second DAG-loading path are involved.
///
/// `ci/select-tests.rs` emits a dependency-CLOSED node set, so pruning cannot
/// drop a genuine dependency — but that is the selector's guarantee, not this
/// function's, so the caller is told how many edges were pruned and how many of
/// the requested tags were not found. An unknown tag is a selector/DAG mismatch
/// and is reported rather than silently ignored.
pub struct Selection {
    pub steps: Vec<Step>,
    pub pruned_edges: usize,
    pub unknown_tags: Vec<String>,
}

pub fn select_lane_nodes(all: Vec<Step>, keep: &std::collections::BTreeSet<String>) -> Selection {
    let present: std::collections::BTreeSet<String> = all.iter().map(|s| s.tag()).collect();
    let unknown_tags: Vec<String> = keep.difference(&present).cloned().collect();
    let mut pruned_edges = 0usize;
    let steps = all
        .into_iter()
        .filter(|s| keep.contains(&s.tag()))
        .map(|mut s| {
            let before = s.deps.len();
            s.deps.retain(|d| keep.contains(d) || !present.contains(d));
            pruned_edges += before - s.deps.len();
            s
        })
        .collect();
    Selection { steps, pruned_edges, unknown_tags }
}

/// DAG tags are `group.job`, so a job containing `.` would produce an ambiguous
/// tag. Corpus labels are shell-command names (`c++filt`, `wc-lines`), none of
/// which contain a dot today, but the mapping is applied rather than assumed.
pub fn sanitize_job(label: &str) -> String {
    label.replace('.', "_")
}

/// Assemble a `DagConfig` from steps, applying the profile-level CPU-time default
/// that the shipped lane JSON does not carry.
/// Load a lane's FULL `DagConfig` -- not just its steps.
///
/// `lane_nodes` returns steps because the fusion path rewrites their tags, but a
/// DAG file is more than a bag of steps: `resource_caps`, `default_step_timeout`,
/// `mem_cap_factor`, `mem_cap_floor_bytes` and `outer_mem_safety_factor` are all
/// top-level, and every one of them silently reverts to `DagConfig::default()` if
/// the caller rebuilds the config instead of carrying it.
pub fn lane_config(root: &Path, lane: &str) -> Result<DagConfig, String> {
    let path = lane_dag_path(root, lane);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    dag_from_json(&text).map_err(|e| format!("invalid DAG {}: {e}", path.display()))
}

/// Assemble a `DagConfig`, CARRYING every top-level field from `base`.
///
/// # Why this takes a base at all
///
/// It used to be `DagConfig { steps, ..Default::default() }`, which loaded a DAG
/// file, kept its steps, and threw its configuration away. That is not a
/// hypothetical: it hung a full validate for 14 minutes at 0% CPU.
/// `ci/dag/portable.json` declares both `hermit_guest` and `manifest_guest`
/// capacities; dropping them left `res_free` evaluating
/// `unwrap_or(0) >= demand` for every step using either resource, so none could
/// ever be admitted. The scheduler's
/// only exit is `running.is_empty() && done + skipped >= steps.len()`, so with
/// work neither runnable nor accounted it slept at 50 ms forever -- no error, no
/// exit, 21 of ~58 nodes done.
///
/// `resource_caps` failed LOUDLY (a visible hang). The quieter one matters more:
/// `default_step_timeout` is 600 s in portable and 120 s in privileged, and
/// reverted to `DEFAULT_STEP_TIMEOUT` (1800 s) -- every step's wall cap loosened
/// 3x and 15x respectively, with nothing to see. `mem_cap_factor`,
/// `mem_cap_floor_bytes` and `outer_mem_safety_factor` happen to equal their
/// defaults today, so they would have broken the first time anyone tuned them.
///
/// Hence: carry the base wholesale, and let [`assert_config_carried`] prove it.
pub fn config_from_base(base: &DagConfig, steps: Vec<Step>, description: &str) -> DagConfig {
    let mut cfg = base.clone();
    cfg.steps = steps;
    cfg.description = description.to_string();
    // The one DELIBERATE divergence: older shipped lane nodes omit cpu_timeout,
    // so supply a load-immune default. A node's own cpu_timeout still wins via
    // effective_cpu_timeout. Recorded here so the audit can exempt it.
    cfg.default_step_cpu_timeout = LANE_DEFAULT_CPU_TIMEOUT_S;
    cfg
}

/// Synthesised plans that have no source DAG file (compat, quick, envelope, ...).
pub fn config_from(steps: Vec<Step>, description: &str) -> DagConfig {
    config_from_base(&DagConfig::default(), steps, description)
}

/// Field-by-field proof that `derived` carried `base`'s configuration.
///
/// Enumerated deliberately rather than derived from a `PartialEq`: a new
/// `DagConfig` field must force a decision here instead of silently defaulting,
/// which is the exact failure this function exists to prevent. `steps` and
/// `description` are expected to differ; `default_step_cpu_timeout` is the one
/// documented divergence above.
pub fn assert_config_carried(base: &DagConfig, derived: &DagConfig) -> Result<(), String> {
    let mut bad: Vec<String> = Vec::new();
    if base.resource_caps != derived.resource_caps {
        bad.push(format!("resource_caps {:?} != {:?}", base.resource_caps, derived.resource_caps));
    }
    if base.mem_cap_factor != derived.mem_cap_factor {
        bad.push(format!("mem_cap_factor {} != {}", base.mem_cap_factor, derived.mem_cap_factor));
    }
    if base.mem_cap_floor_bytes != derived.mem_cap_floor_bytes {
        bad.push(format!("mem_cap_floor_bytes {} != {}", base.mem_cap_floor_bytes, derived.mem_cap_floor_bytes));
    }
    if base.outer_mem_safety_factor != derived.outer_mem_safety_factor {
        bad.push(format!("outer_mem_safety_factor {} != {}", base.outer_mem_safety_factor, derived.outer_mem_safety_factor));
    }
    if base.default_step_timeout != derived.default_step_timeout {
        bad.push(format!("default_step_timeout {} != {}", base.default_step_timeout, derived.default_step_timeout));
    }
    if base.default_jobs_flag != derived.default_jobs_flag {
        bad.push(format!("default_jobs_flag {:?} != {:?}", base.default_jobs_flag, derived.default_jobs_flag));
    }
    if base.default_jobs_env != derived.default_jobs_env {
        bad.push(format!("default_jobs_env {:?} != {:?}", base.default_jobs_env, derived.default_jobs_env));
    }
    if base.default_step_mem_cap_bytes != derived.default_step_mem_cap_bytes {
        bad.push(format!("default_step_mem_cap_bytes {:?} != {:?}", base.default_step_mem_cap_bytes, derived.default_step_mem_cap_bytes));
    }
    if base.default_step_cpu_count != derived.default_step_cpu_count {
        bad.push(format!("default_step_cpu_count {:?} != {:?}", base.default_step_cpu_count, derived.default_step_cpu_count));
    }
    if bad.is_empty() { Ok(()) } else { Err(bad.join("; ")) }
}

/// FAIL CLOSED on capacity that can never be granted.
///
/// A step demanding a resource the config does not cap is unschedulable FOREVER,
/// and the scheduler expresses that as an infinite 50 ms sleep rather than an
/// error. Refusing up front converts a silent 14-minute hang into a named
/// refusal before a single node runs.
pub fn ungrantable_resources(cfg: &DagConfig) -> Vec<String> {
    let mut bad = Vec::new();
    for s in &cfg.steps {
        for (r, n) in &s.hint.resources {
            let cap = cfg.resource_caps.get(r).copied().unwrap_or(0);
            if cap < *n {
                bad.push(format!("{} demands {r}={n} but resource_caps grants {cap}", s.tag()));
            }
        }
    }
    bad
}

/// Fail-closed audit: every node in a plan must declare a wall timeout, a CPU
/// budget (its own or the config default), and a memory cap.
///
/// This is the guard that keeps the module doc's claim true as nodes are added.
/// Without it, a future node added without hints would run UNBOXED while the
/// driver still printed "cgroup boxing ACTIVE" — a green that verified less than
/// it claimed, which is precisely the failure class this port exists to remove.
///
/// Returns the tags of any nodes that are not fully declared.
pub fn undeclared_nodes(cfg: &DagConfig) -> Vec<String> {
    cfg.steps
        .iter()
        .filter(|s| {
            let mem = s.hint.hard_mem_max_bytes.is_some() || s.hint.rss_baseline_bytes.is_some();
            let cpu = s.cpu_timeout > 0 || cfg.default_step_cpu_timeout > 0;
            let wall = s.timeout > 0;
            !(mem && cpu && wall)
        })
        .map(|s| s.tag())
        .collect()
}
