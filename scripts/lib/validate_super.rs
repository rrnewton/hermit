// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Generator and reporting support for the committed super population.
//!
//! Static super nodes are authored directly in `ci/dag/validate.json`. The
//! maintenance generator calls this module only for the namespaced `superstress`
//! partition; runtime validation selects the committed `super` label.

use std::path::Path;

use dagrun::model::Step;
use dagrun::model::StepOutcome;

use crate::validate_plan::node;
use crate::validate_plan::shell_quote;

/// `GATE_TIMEOUT_SECONDS` (validate.sh:400). A generated super node without
/// an explicit override inherits this historical default before it is committed.
pub const DEFAULT_GATE_TIMEOUT_S: i64 = 600;

/// `SUPER_REPETITIONS` (validate.sh:682).
pub const SUPER_REPETITIONS_DEFAULT: i64 = 20;

/// `STRICT_COMPAT_TIMEOUT` (validate.sh:1091) — the per-probe wall bound the
/// bash imposed with the `timeout` binary. Here it is the node's wall cap, so a
/// hung repetition is killed and reported by the runner rather than by a nested
/// `timeout` whose exit code the runner would have to reinterpret.
pub const SUPER_PROBE_TIMEOUT_S: i64 = 60;

/// CPU budget for one stress repetition. These are sub-second guest runs; a CPU
/// cap is what catches a spin that the wall cap would only catch at 60s.
const SUPER_PROBE_CPU_TIMEOUT_S: i64 = 120;
const SUPER_PROBE_MEM_BYTES: i64 = 4 * 1024 * 1024 * 1024;

// --------------------------------------------------------------------- stress

/// The five probes `run_super_stress_suite` names (validate.sh:2686, :2695, :2702).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StressProbe {
    PtraceStrictVerify,
    PtracePipeline,
    PtraceRecordReplay,
    KvmVerify,
    DbtVerify,
}

impl StressProbe {
    pub fn slug(self) -> &'static str {
        match self {
            StressProbe::PtraceStrictVerify => "ptrace-strict-verify",
            StressProbe::PtracePipeline => "ptrace-pipeline",
            StressProbe::PtraceRecordReplay => "ptrace-record-replay",
            StressProbe::KvmVerify => "kvm-verify",
            StressProbe::DbtVerify => "dbt-verify",
        }
    }

    fn job_stem(self) -> String {
        self.slug().replace('-', "_")
    }

    /// The availability node this probe depends on, if any.
    fn availability_job(self) -> Option<&'static str> {
        match self {
            StressProbe::KvmVerify => Some("kvm_available"),
            StressProbe::DbtVerify => Some("dbt_available"),
            StressProbe::PtraceStrictVerify
            | StressProbe::PtracePipeline
            | StressProbe::PtraceRecordReplay => None,
        }
    }

    /// True when a failure of this probe must NOT turn the suite red.
    ///
    /// See the module doc: `backend_selector_supported` is undefined, so KVM and
    /// DBT stress have never actually been measured by `validate.sh`. Their
    /// first measurement is reported, not ratcheted.
    pub fn nonblocking(self) -> bool {
        matches!(self, StressProbe::KvmVerify | StressProbe::DbtVerify)
    }

    /// One repetition's shell command, reproducing `super_probe_command`
    /// (validate.sh:2589). The outer `timeout` binary is dropped because the
    /// node's own wall cap enforces the same bound and the runner then reports a
    /// TYPED timeout instead of an opaque exit 124.
    fn command(self, iteration: i64, release_bin: &str, debug_bin: &str, tmp: &Path) -> String {
        let rel = shell_quote(release_bin);
        let dbg = shell_quote(debug_bin);
        match self {
            StressProbe::PtraceStrictVerify => format!(
                "{rel} run --strict --verify -- /bin/echo hermit-super-{iteration} </dev/null"
            ),
            StressProbe::PtracePipeline => format!(
                "{rel} run --strict --verify -- bash -c 'yes hermit | head -n 64 | sha256sum' </dev/null"
            ),
            StressProbe::PtraceRecordReplay => {
                let dir = shell_quote(
                    &tmp.join(format!("super-record-{iteration}")).to_string_lossy(),
                );
                // The bash removed the data dir before AND after, preserving the
                // record phase's exit status across the second removal.
                format!(
                    "rm -rf {dir}; {rel} record start --verify --data-dir {dir} -- \
                     /bin/echo hermit-super-record-{iteration} </dev/null; \
                     status=$?; rm -rf {dir}; exit $status"
                )
            }
            StressProbe::KvmVerify => format!(
                "{dbg} run --backend kvm --verify -- /bin/echo hermit-super-kvm-{iteration} </dev/null"
            ),
            StressProbe::DbtVerify => format!(
                "{dbg} run --backend dbt --verify -- /bin/echo hermit-super-dbt-{iteration} </dev/null"
            ),
        }
    }
}

pub const STRESS_PROBES: &[StressProbe] = &[
    StressProbe::PtraceStrictVerify,
    StressProbe::PtracePipeline,
    StressProbe::PtraceRecordReplay,
    StressProbe::KvmVerify,
    StressProbe::DbtVerify,
];

/// The two backend-availability nodes.
///
/// `kvm_backend_available` (validate.sh:2272) is a readable+writable `/dev/kvm`;
/// `dbt_backend_available` (validate.sh:2276) is a real probe run, which is why
/// it must be a node — at plan time the debug binary does not exist yet.
fn availability_nodes(debug_bin: &str, build_dep: &str) -> Vec<Step> {
    let dbg = shell_quote(debug_bin);
    vec![
        node(
            "superstress",
            "kvm_available",
            "KVM backend availability (gates the KVM stress rows)",
            "test -r /dev/kvm && test -w /dev/kvm".to_string(),
            vec![build_dep.to_string()],
            30,
            30,
            256 * 1024 * 1024,
        ),
        node(
            "superstress",
            "dbt_available",
            "DBT backend availability (gates the DBT stress rows)",
            format!(
                "{dbg} --log=info run --backend dbt --strict --verify -- \
                 /bin/echo hermit-dbt-probe </dev/null >/dev/null 2>&1"
            ),
            vec![build_dep.to_string()],
            60,
            120,
            SUPER_PROBE_MEM_BYTES,
        ),
    ]
}

/// Build every stress node: two availability probes plus `reps` repetitions of
/// each of the five probes.
pub fn stress_nodes(
    release_bin: &str,
    debug_bin: &str,
    tmp: &Path,
    reps: i64,
    release_dep: &str,
    debug_dep: &str,
) -> Vec<Step> {
    let mut out = availability_nodes(debug_bin, debug_dep);
    for probe in STRESS_PROBES {
        let stem = probe.job_stem();
        let base_dep = match probe {
            StressProbe::KvmVerify | StressProbe::DbtVerify => debug_dep,
            _ => release_dep,
        };
        let mut deps = vec![base_dep.to_string()];
        if let Some(av) = probe.availability_job() {
            deps.push(format!("superstress.{av}"));
        }
        for i in 1..=reps {
            out.push(node(
                "superstress",
                &format!("{stem}_{i:02}"),
                &format!("super stress {} repetition {i}/{reps}", probe.slug()),
                probe.command(i, release_bin, debug_bin, tmp),
                deps.clone(),
                SUPER_PROBE_TIMEOUT_S,
                SUPER_PROBE_CPU_TIMEOUT_S,
                SUPER_PROBE_MEM_BYTES,
            ));
        }
    }
    out
}

/// Per-probe pass rate, derived from typed outcomes.
#[derive(Clone, Debug)]
pub struct ProbeRate {
    pub probe: StressProbe,
    pub passed: usize,
    /// Repetitions that actually ran (a skipped dependent never ran).
    pub ran: usize,
    pub planned: usize,
}

/// Recompute `run_super_probe`'s report from typed `StepOutcome`s.
///
/// The bash scraped its own tee'd text file (`$VALIDATION_TMP_DIR/super-report`);
/// this reads the runner's structured verdicts, so the printed rate and the
/// blocking decision cannot disagree with what actually ran.
pub fn stress_rates(outcomes: &[StepOutcome], reps: i64) -> Vec<ProbeRate> {
    let mut rates = Vec::new();
    for probe in STRESS_PROBES {
        let stem = probe.job_stem();
        let prefix = format!("superstress.{stem}_");
        let mut passed = 0usize;
        let mut ran = 0usize;
        for o in outcomes {
            if !o.tag.starts_with(&prefix) {
                continue;
            }
            if o.aborted {
                continue;
            }
            ran += 1;
            if o.ok {
                passed += 1;
            }
        }
        rates.push(ProbeRate { probe: *probe, passed, ran, planned: reps as usize });
    }
    rates
}

/// Print the pass-rate table and return the BLOCKING failure count.
///
/// A probe is blocking iff it is a ptrace probe (the three the bash actually
/// measured) and it did not pass every planned repetition. KVM/DBT rates are
/// printed with the reason they are nonblocking, so the number is visible
/// without silently becoming a gate on its first appearance.
pub fn stress_verdict(rates: &[ProbeRate], reps: i64, jobs: i64, host_cpus: usize) -> usize {
    println!("\n== Super stress pass rates ==");
    println!("Repetitions: {reps}; scheduler width: {jobs}; online CPUs: {host_cpus}");
    let mut blocking = 0usize;
    for r in rates {
        let slug = r.probe.slug();
        if r.ran == 0 {
            println!("  SKIP {slug:<24} backend unavailable (availability node failed; 0/{reps} ran)");
            continue;
        }
        let pct = 100 * r.passed / r.planned.max(1);
        if r.passed == r.planned {
            println!("  ✅ {slug:<24} {}/{} (100%)", r.passed, r.planned);
        } else if r.probe.nonblocking() {
            println!(
                "  ⚠️  {slug:<24} {}/{} ({pct}%) FLAKY/FAILING — NONBLOCKING: this row was dead \
                 code in validate.sh (`backend_selector_supported` is undefined, so the guard was \
                 always false) and has never been measured; reporting it, not ratcheting it.",
                r.passed, r.planned
            );
        } else {
            println!("  ⚠️  {slug:<24} {}/{} ({pct}%) FLAKY/FAILING", r.passed, r.planned);
            blocking += 1;
        }
    }
    blocking
}

// ----------------------------------------------------------------- self-test

/// Focused controls for the reporting policy that remains live after the
/// committed DAG became the sole source of super-plan construction.
pub fn self_test() -> Result<String, String> {
    let reps = 2;
    let all_green = STRESS_PROBES
        .iter()
        .copied()
        .map(|probe| ProbeRate {
            probe,
            passed: reps as usize,
            ran: reps as usize,
            planned: reps as usize,
        })
        .collect::<Vec<_>>();
    if stress_verdict(&all_green, reps, 1, 1) != 0 {
        return Err("super stress verdict: an all-passing population must be accepted".into());
    }

    let mut ptrace_miss = all_green.clone();
    ptrace_miss[0].passed -= 1;
    if stress_verdict(&ptrace_miss, reps, 1, 1) != 1 {
        return Err("super stress verdict: a ptrace miss must remain blocking".into());
    }

    let mut kvm_miss = all_green;
    let kvm = kvm_miss
        .iter_mut()
        .find(|rate| rate.probe == StressProbe::KvmVerify)
        .ok_or_else(|| "super stress verdict: KVM control is absent".to_string())?;
    kvm.passed -= 1;
    if stress_verdict(&kvm_miss, reps, 1, 1) != 0 {
        return Err("super stress verdict: the first KVM measurement must remain nonblocking".into());
    }

    Ok("super stress verdict: all-pass accepted; ptrace miss blocks; KVM miss reports without blocking".into())
}

/// Environment overrides this module honors, for the plan banner.
pub fn repetitions() -> i64 {
    std::env::var("SUPER_REPETITIONS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(SUPER_REPETITIONS_DEFAULT)
}
