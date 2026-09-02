// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Reporting for the committed super stress population.
//!
//! Plan construction belongs to `ci/dag/validate.json`. This module only
//! interprets the runner's typed outcomes and prints the existing pass-rate
//! summary; it must not carry a second copy of the graph.
use dagrun::model::StepOutcome;

/// Default number of repetitions in the committed super stress population.
pub const SUPER_REPETITIONS_DEFAULT: i64 = 20;
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

    /// True when a failure of this probe must NOT turn the suite red.
    ///
    /// See the module doc: `backend_selector_supported` is undefined, so KVM and
    /// DBT stress have never actually been measured by `validate.sh`. Their
    /// first measurement is reported, not ratcheted.
    pub fn nonblocking(self) -> bool {
        matches!(self, StressProbe::KvmVerify | StressProbe::DbtVerify)
    }
}

pub const STRESS_PROBES: &[StressProbe] = &[
    StressProbe::PtraceStrictVerify,
    StressProbe::PtracePipeline,
    StressProbe::PtraceRecordReplay,
    StressProbe::KvmVerify,
    StressProbe::DbtVerify,
];

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

/// Focused controls for the still-live grouped stress verdict.
///
/// Plan construction lives in the committed DAG, but these reporting semantics
/// remain policy: every planned ptrace repetition must pass, while the first
/// measurements of KVM and DBT remain visible without becoming a gate.
pub fn self_test() -> Result<String, String> {
    let reps = 2;
    let all_green = STRESS_PROBES
        .iter()
        .copied()
        .map(|probe| ProbeRate { probe, passed: reps as usize, ran: reps as usize, planned: reps as usize })
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
