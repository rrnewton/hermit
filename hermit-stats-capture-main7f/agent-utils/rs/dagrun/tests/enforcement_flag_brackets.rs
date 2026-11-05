//! #79 derived-enforcement-manifest: the scheduler's guards follow the published registry.
//!
//! Each of these guards is advertised by the `capabilities` manifest, and each now asks
//! `capabilities::is_enforced` at the point where it acts. This file proves the coupling in the
//! only way that means anything: flip one flag and observe that BOTH the manifest text and the
//! running behaviour move.
//!
//! WHY ONE TEST FUNCTION IN ITS OWN BINARY. `with_registry_override` flips a PROCESS-WIDE
//! registry, so while a bracket is open every thread in the process sees the flipped value. A
//! test harness runs test functions concurrently, and the guards exercised here are turned OFF
//! for whole seconds at a time, so an unrelated concurrent test that depends on a step being cut
//! would fail for a reason that has nothing to do with it. Each integration test file is its own
//! process and everything here is one sequential function, so the windows cannot overlap
//! anything. The library's own unit tests take the matching `with_registry_pinned` turn.

use std::collections::BTreeMap;
use std::sync::Arc;

use dagrun::capabilities::{
    enforcement_manifest, with_lane_registry_override, with_registry_override, Lane,
};
use dagrun::model::{DagConfig, ResourceHint, Step};
use dagrun::scheduler::run_dag_boxed_limited;
use dagrun::CgroupManager;

/// A boxed manager whose cgroup reports a fixed OOM-kill count and a fixed consumed CPU time.
/// Both readings are what a guard that only fires on a cgroup measurement needs in order to be
/// observable without a real cgroup.
struct SyntheticReadings {
    oom_kills: i64,
    cpu_usage_usec: i64,
}

impl CgroupManager for SyntheticReadings {
    fn enabled(&self) -> bool {
        true
    }

    fn prepare_command(
        &self,
        _tag: &str,
        cmd: &str,
        _mem_max: Option<i64>,
        _cpu_count: Option<i64>,
    ) -> String {
        cmd.to_string()
    }

    fn kill(&self, _tag: &str) -> bool {
        false
    }

    fn cleanup(&self, _tag: &str) {}

    fn oom_kills(&self, _tag: &str) -> i64 {
        self.oom_kills
    }

    /// The scheduler reads `memory.events` ONCE and takes the OOM count from it, so this is the
    /// method the oom_detection bracket actually exercises; answering only `oom_kills` would
    /// leave that leg silently inert.
    fn memory_events(&self, _tag: &str) -> Option<BTreeMap<String, i64>> {
        Some(BTreeMap::from([
            ("oom_kill".to_string(), self.oom_kills),
            ("oom".to_string(), self.oom_kills),
            ("low".to_string(), 0),
            ("high".to_string(), 0),
            ("max".to_string(), 0),
        ]))
    }

    fn peak_bytes(&self, _tag: &str) -> Option<i64> {
        None
    }

    fn cpu_stats(&self, _tag: &str) -> Option<BTreeMap<String, i64>> {
        Some(BTreeMap::from([
            ("usage_usec".to_string(), self.cpu_usage_usec),
            ("user_usec".to_string(), self.cpu_usage_usec),
            ("system_usec".to_string(), 0),
            ("throttled_usec".to_string(), 0),
        ]))
    }

    fn cpu_pressure(&self, _tag: &str) -> Option<BTreeMap<String, f64>> {
        None
    }

    fn thread_count(&self, _tag: &str) -> Option<i64> {
        None
    }

    fn kill_all_remaining(&self) -> i64 {
        0
    }
}

fn one_step(cmd: &str, timeout: i64, cpu_timeout: i64) -> DagConfig {
    let step = Step {
        group: "g".into(),
        job: "s".into(),
        desc: String::new(),
        description: String::new(),
        labels: Vec::new(),
        cmd: cmd.into(),
        cmdtype: dagrun::CmdType::Unknown,
        manifest: None,
        result_manifests: None,
        integration_test_binaries: None,
        deps: Vec::new(),
        env: BTreeMap::new(),
        hint: ResourceHint::default(),
        networkonly: false,
        engine_only: false,
        delegated_children: false,
        timeout,
        cpu_timeout,
        jobs_flag: None,
        jobs_env: None,
        skip_reason: None,
        write_domains: None,
        write_domain_guarantee: None,
        explains: Vec::new(),
        fail_fast_family: None,
    };
    DagConfig {
        steps: vec![step],
        default_step_cpu_timeout: 0,
        ..Default::default()
    }
}

#[test]
fn scheduler_guards_follow_their_published_capability_flags() {
    // ---- wall_timeout: the per-step wait deadline ----------------------------------------
    let slow = one_step("sleep 2", 1, 0);
    assert!(enforcement_manifest().contains("\"wall_timeout\":true"));
    let enforced = run_dag_boxed_limited(&slow, 1, 1, false, 0, None);
    assert!(
        !enforced.ok,
        "a 2s step under a 1s wall ceiling must be cut, but the run succeeded"
    );

    let unenforced = with_registry_override("wall_timeout", false, || {
        // ABSENCE of the `true`, not presence of a `false`: the manifest publishes two
        // lanes, so a bare `contains(":false")` can hold whatever the bracket did.
        assert!(!enforcement_manifest().contains("\"wall_timeout\":true"));
        run_dag_boxed_limited(&slow, 1, 1, false, 0, None)
    });
    assert!(
        unenforced.ok,
        "with wall_timeout declared unenforced the step must be allowed to finish; it was still \
         cut, so the manifest and the guard can disagree: {}",
        unenforced.outcomes[0].reason
    );

    // ---- oom_detection: the post-step memory.events read ---------------------------------
    let failing = one_step("exit 1", 30, 0);
    let ooming = || {
        Arc::new(SyntheticReadings {
            oom_kills: 2,
            cpu_usage_usec: 0,
        })
    };
    assert!(enforcement_manifest().contains("\"oom_detection\":true"));
    let enforced = run_dag_boxed_limited(&failing, 1, 1, false, 0, Some(ooming()));
    assert!(
        enforced.outcomes[0].reason.contains("OOM-KILLED"),
        "{}",
        enforced.outcomes[0].reason
    );

    let unenforced = with_registry_override("oom_detection", false, || {
        // ABSENCE of the `true`, not presence of a `false`: the manifest publishes two
        // lanes, so a bare `contains(":false")` can hold whatever the bracket did.
        assert!(!enforcement_manifest().contains("\"oom_detection\":true"));
        run_dag_boxed_limited(&failing, 1, 1, false, 0, Some(ooming()))
    });
    assert!(
        !unenforced.outcomes[0].reason.contains("OOM-KILLED"),
        "memory.events was still consulted although oom_detection is declared unenforced: {}",
        unenforced.outcomes[0].reason
    );

    // ---- cpu_timeout: the 1 Hz cpu.stat monitor that reaps -------------------------------
    // The wall ceiling is generous here, so only the CPU-time guard can cut this step.
    let burning = one_step("sleep 4", 60, 1);
    // Ten CPU-seconds already consumed, against a one-second budget.
    let hot = || {
        Arc::new(SyntheticReadings {
            oom_kills: 0,
            cpu_usage_usec: 10_000_000,
        })
    };
    assert!(enforcement_manifest().contains("\"cpu_timeout\":true"));
    let enforced = run_dag_boxed_limited(&burning, 1, 1, false, 0, Some(hot()));
    assert!(!enforced.ok);
    assert!(
        enforced.outcomes[0].reason.contains("CPU-TIMEOUT"),
        "{}",
        enforced.outcomes[0].reason
    );

    let unenforced = with_registry_override("cpu_timeout", false, || {
        // ABSENCE of the `true`, not presence of a `false`: the manifest publishes two
        // lanes, so a bare `contains(":false")` can hold whatever the bracket did.
        assert!(!enforcement_manifest().contains("\"cpu_timeout\":true"));
        run_dag_boxed_limited(&burning, 1, 1, false, 0, Some(hot()))
    });
    assert!(
        unenforced.ok,
        "the step was still reaped over its CPU budget although cpu_timeout is declared \
         unenforced: {}",
        unenforced.outcomes[0].reason
    );

    // ---- the LANE reaches the guard site -------------------------------------------------
    // #75 cpu-timeout-unboxed-fallback: the manifest has a column per lane, and the column is
    // only worth publishing if the guard reads the one the run is actually on. `wall_timeout` is
    // the key that is true on BOTH lanes, so flipping just the uncontained column must change an
    // unboxed run and leave a boxed one alone. If the guard site ignored its lane argument, one
    // of these two assertions is false whichever way it guessed.
    let slow_unboxed = one_step("sleep 2", 1, 0);
    with_lane_registry_override("wall_timeout", false, Lane::Uncontained, || {
        let manifest = enforcement_manifest();
        assert!(manifest.contains("\"wall_timeout\":true"), "{manifest}");
        assert!(manifest.contains("\"wall_timeout\":false"), "{manifest}");

        let unboxed = run_dag_boxed_limited(&slow_unboxed, 1, 1, false, 0, None);
        assert!(
            unboxed.ok,
            "the UNCONTAINED column says no wall ceiling, but the unboxed step was still cut: {}",
            unboxed.outcomes[0].reason
        );

        let boxed = run_dag_boxed_limited(
            &slow_unboxed,
            1,
            1,
            false,
            0,
            Some(Arc::new(SyntheticReadings {
                oom_kills: 0,
                cpu_usage_usec: 0,
            })),
        );
        assert!(
            !boxed.ok,
            "only the UNCONTAINED column was flipped, so a boxed step must still be cut"
        );
    });
}
