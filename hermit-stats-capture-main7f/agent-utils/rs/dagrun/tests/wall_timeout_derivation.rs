//! A wall backstop should be derived from the CPU budget, not baked into the graph.
//!
//! `Step.timeout` used to default to a hardcoded 1800 the moment a document was loaded, which is
//! exactly the load-sensitive number a derivation exists to remove: it is the same on a laptop
//! and on a 300-core host, and it tells you nothing about the step. parallel-experiment-runner
//! (Python-only) already solved this — explicit wall wins, else 3x the CPU budget, else the
//! default — and this engine reuses that idiom rather than inventing a second policy.
//!
//! Two design questions had to be ANSWERED, not merely implemented, and both are pinned below:
//!
//! * derive from the DECLARED cpu_timeout, never the canonical (default-filled) one, or every
//!   undeclared step would silently drop from a 1800-second ceiling to a 30-second one; and
//! * derive from the PLATFORM-SCALED budget, or the backstop would race the CPU guard on exactly
//!   the slow platform `cpu_timeout_multiplier` exists for.
//!
//! A third answer was added after review: the derivation is FLOORED at the default, so it can only
//! ever move a step's ceiling away from its CPU guard. Unfloored it retimed every already-authored
//! step that declared a CPU budget, and for anything that blocks — a fetch, a lock wait — wall
//! time is unbounded relative to CPU time, so three times a small budget is not a hang.
//!
//! This is the Rust half of `py/tests/test_wall_timeout_derivation.py`.

use dagrun::model::{
    canonical_cpu_timeout, resolved_wall_timeout, DEFAULT_SMALL_CPU_TIMEOUT, DEFAULT_STEP_TIMEOUT,
    WALL_CPU_BACKSTOP_FACTOR,
};
use dagrun::{dag_from_json, dag_to_json, steps_violating_run_timeout, DagConfig, Step};

fn step(timeout: i64, cpu_timeout: i64) -> Step {
    Step {
        group: "g".into(),
        job: "a".into(),
        desc: "d".into(),
        description: String::new(),
        labels: Vec::new(),
        cmd: "true".into(),
        cmdtype: dagrun::CmdType::Unknown,
        manifest: None,
        result_manifests: None,
        integration_test_binaries: None,
        deps: Vec::new(),
        env: Default::default(),
        hint: Default::default(),
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
    }
}

#[test]
fn the_factor_is_three_as_the_established_idiom_spells_it() {
    // The Python `parallel_experiment_runner` twin cannot be imported here, so the value is
    // pinned literally on this side and the two are held equal by the Python test.
    assert_eq!(WALL_CPU_BACKSTOP_FACTOR, 3);
}

#[test]
fn an_explicit_step_budget_wins_over_everything() {
    assert_eq!(resolved_wall_timeout(&step(42, 7), 600, 2.0), 42);
}

#[test]
fn a_document_default_wins_over_the_derivation() {
    // An author who wrote a document-wide number said something; the derivation must not
    // second-guess it.
    assert_eq!(resolved_wall_timeout(&step(0, 7), 600, 1.0), 600);
}

#[test]
fn a_declared_cpu_budget_derives_the_backstop() {
    // 900 CPU-seconds is the case the rule exists for: a baked-in 1800 is only 2x that budget,
    // and the CPU guard can reach it. 2700 restores the 3x margin.
    assert_eq!(resolved_wall_timeout(&step(0, 900), 0, 1.0), 2700);
}

#[test]
fn a_small_cpu_budget_does_not_retime_an_already_authored_step() {
    // THE REGRESSION THE FLOOR PREVENTS. `{"cmd": "git fetch ...", "cpu_timeout": 5}` burns ~5
    // CPU-seconds and blocks for minutes on the network. Unfloored, rule 3 gives it a 15-second
    // wall ceiling and SIGTERMs it as a hang — a silent retiming of every existing step that
    // declared a CPU budget. Wall time is unbounded relative to CPU time for anything that
    // blocks, so the derivation is allowed to loosen and never to tighten.
    assert_ne!(resolved_wall_timeout(&step(0, 5), 0, 1.0), 15);
    assert_eq!(resolved_wall_timeout(&step(0, 5), 0, 1.0), 1800);
    assert_eq!(DEFAULT_STEP_TIMEOUT, 1800);
    // A networkonly step is the same story, said by the schema: it is DECLARED to depend on a
    // resource whose latency has nothing to do with its CPU budget.
    let mut net = step(0, 5);
    net.networkonly = true;
    assert_eq!(resolved_wall_timeout(&net, 0, 1.0), 1800);
}

#[test]
fn the_floor_is_exactly_where_the_derivation_overtakes_the_default() {
    // Named literally on both sides of the boundary, so "always return 1800" and "never floor"
    // are each caught by one of these two lines.
    assert_eq!(resolved_wall_timeout(&step(0, 600), 0, 1.0), 1800);
    assert_eq!(resolved_wall_timeout(&step(0, 601), 0, 1.0), 1803);
}

#[test]
fn the_derivation_tracks_the_platform_scaled_budget() {
    // 400 CPU-seconds on a platform 2.5x slower is a 1000-second enforced budget, so the wall
    // backstop is 3000 and keeps its 3x margin. Pinned to the DECLARED 400 it would be 1200,
    // which the floor would then round back up to 1800 — BELOW the 1000-second enforced guard's
    // 3x margin, i.e. racing it on exactly the platform the multiplier exists for.
    assert_eq!(resolved_wall_timeout(&step(0, 400), 0, 2.5), 3000);
}

#[test]
fn a_step_that_declared_nothing_keeps_the_1800_second_backstop() {
    // THE ANSWER TO THE OPEN QUESTION. `canonical_cpu_timeout` fills in the small 10-second
    // default for such a step, and deriving from THAT would give it a 30-second wall ceiling.
    let s = step(0, 0);
    assert_eq!(canonical_cpu_timeout(&s, DEFAULT_SMALL_CPU_TIMEOUT), 10);
    assert_eq!(resolved_wall_timeout(&s, 0, 1.0), DEFAULT_STEP_TIMEOUT);
    assert_eq!(DEFAULT_STEP_TIMEOUT, 1800);
}

#[test]
fn the_sentinel_is_absence_and_round_trips_as_absence() {
    let doc = r#"{"steps": [{"group": "g", "job": "a", "cmd": "true", "cpu_timeout": 7}]}"#;
    let cfg = dag_from_json(doc).expect("load");
    assert_eq!(cfg.steps[0].timeout, 0);
    assert_eq!(cfg.default_step_timeout, 0);
    let emitted = dag_to_json(&cfg);
    assert!(
        !emitted.contains("\"timeout\""),
        "0 written out would read as 'no wall bound': {emitted}"
    );
    assert!(!emitted.contains("\"default_step_timeout\""));
    let reloaded = dag_from_json(&emitted).expect("reload");
    assert_eq!(dag_to_json(&reloaded), emitted);
    assert_eq!(reloaded.steps[0].timeout, 0);
}

#[test]
fn a_declared_budget_still_round_trips_as_a_number() {
    // The other side: omission must not become "always omit".
    let doc = r#"{"steps": [{"group": "g", "job": "a", "cmd": "true", "timeout": 42}]}"#;
    let cfg = dag_from_json(doc).expect("load");
    assert_eq!(cfg.steps[0].timeout, 42);
    assert!(dag_to_json(&cfg).contains("\"timeout\": 42"));
}

#[test]
fn a_document_default_still_round_trips_as_a_number() {
    let doc =
        r#"{"default_step_timeout": 600, "steps": [{"group": "g", "job": "a", "cmd": "true"}]}"#;
    let cfg = dag_from_json(doc).expect("load");
    assert_eq!(cfg.default_step_timeout, 600);
    // The loader materializes it into the step, as it always has.
    assert_eq!(cfg.steps[0].timeout, 600);
    let emitted = dag_to_json(&cfg);
    assert!(emitted.contains("\"default_step_timeout\": 600"));
    assert_eq!(
        dag_to_json(&dag_from_json(&emitted).expect("reload")),
        emitted
    );
}

#[test]
fn the_run_budget_ordering_is_checked_on_the_resolved_value() {
    // A 0 sentinel passes `>= run_timeout_s` trivially, so the fail-closed inner-below-outer
    // ordering has to be expressed on the value the step will actually run under.
    let cfg = DagConfig {
        steps: vec![step(0, 900)], // derives a 2700-second wall backstop
        ..Default::default()
    };
    assert_eq!(
        steps_violating_run_timeout(&cfg, 2000),
        vec![("g.a".to_string(), 2700)]
    );
    assert!(steps_violating_run_timeout(&cfg, 3000).is_empty());
}

#[test]
fn an_undeclared_step_is_still_caught_by_the_run_budget_check() {
    // Its resolved bound is 1800, so a 900-second run budget must still refuse it. Before the
    // resolved value was used, its declared 0 would have sailed through.
    let cfg = DagConfig {
        steps: vec![step(0, 0)],
        ..Default::default()
    };
    assert_eq!(
        steps_violating_run_timeout(&cfg, 900),
        vec![("g.a".to_string(), 1800)]
    );
}

#[test]
fn the_derivation_does_not_quietly_admit_a_graph_the_ordering_check_refused() {
    // The other direction, RESTATED after the floor landed. An unfloored rule 3 would derive a
    // 15-second ceiling for this step and let a 60-second run budget accept a graph that has
    // always been refused — a loosening of a fail-closed pre-flight check, obtained by pretending
    // a network-blocked step cannot outlive 3x its CPU budget. It is still refused, at 1800.
    let cfg = DagConfig {
        steps: vec![step(0, 5)],
        ..Default::default()
    };
    assert_eq!(
        steps_violating_run_timeout(&cfg, 60),
        vec![("g.a".to_string(), 1800)]
    );
}
