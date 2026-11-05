//! The capability manifest must describe the lane the run is actually on.
//!
//! Exact CPU-time enforcement remains a cgroup-only guarantee. The uncontained lane now attempts
//! a best-effort procfs process-group lower bound, but it can miss processes that leave the group
//! and CPU from exited descendants before their parent reaps them. The manifest therefore stays
//! false while the scheduler names and tests the weaker attempt.
//!
//! The manifest is no longer a literal at all: it is generated from
//! `capabilities::ENFORCEMENT_REGISTRY`, whose entries carry one flag per lane. That is what makes
//! the two columns below checkable AGAINST BEHAVIOUR rather than only against each other — see
//! `enforcement_flag_brackets.rs`, which flips a flag and watches the guard move.
//!
//! The tables below are written out by hand rather than derived from the registry under test: a
//! test parametrised over the value it is meant to protect asserts nothing, and re-serializing the
//! registry would agree with any registry at all. This file is the Rust half of
//! `py/tests/test_enforcement_manifest.py`; `make cross` additionally holds the two engines'
//! manifests byte-identical.

use dagrun::capabilities::{enforcement_manifest, with_registry_pinned, Lane};
use dagrun::is_enforced;
use serde_json::Value;

/// What each guard is worth on each lane. Duplicating the production table is the point.
const CONTAINED: &[(&str, bool)] = &[
    ("cpu_affinity", true),
    ("cpu_bandwidth", true),
    ("cpu_timeout", true),
    ("memory_max", true),
    ("oom_detection", true),
    ("pids_guard", false),
    ("run_timeout", true),
    ("wall_timeout", true),
    ("write_domains", true),
];

const UNCONTAINED: &[(&str, bool)] = &[
    // `--cores` REFUSES on this lane rather than degrading, so the guard is not in force here.
    ("cpu_affinity", false),
    ("cpu_bandwidth", false),
    // No cgroup-equivalent guarantee; the procfs lower bound remains non-contractual.
    ("cpu_timeout", false),
    ("memory_max", false),
    ("oom_detection", false),
    ("pids_guard", false),
    // Scheduler-side wall bounds and a pre-execution declaration check: no cgroup needed.
    ("run_timeout", true),
    ("wall_timeout", true),
    ("write_domains", true),
];

fn published() -> String {
    with_registry_pinned(enforcement_manifest)
}

fn manifest() -> Value {
    serde_json::from_str(&published()).expect("the manifest must be valid JSON")
}

fn lane(name: &str) -> Value {
    manifest()
        .get(name)
        .unwrap_or_else(|| panic!("the manifest must carry a {name:?} lane"))
        .clone()
}

fn check_lane(name: &str, expected: &[(&str, bool)]) {
    let observed = lane(name);
    let map = observed
        .as_object()
        .unwrap_or_else(|| panic!("{name} must be an object"));
    let keys: Vec<&str> = map.keys().map(String::as_str).collect();
    let want_keys: Vec<&str> = expected.iter().map(|(k, _)| *k).collect();
    assert_eq!(keys, want_keys, "{name}: key set / ordering");
    for (key, want) in expected {
        assert_eq!(
            map.get(*key).and_then(Value::as_bool),
            Some(*want),
            "{name}.{key}"
        );
    }
}

#[test]
fn the_manifest_declares_both_lanes_and_nothing_else() {
    let top = manifest();
    let keys: Vec<String> = top
        .as_object()
        .expect("the manifest must be an object")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        keys,
        vec!["contained".to_string(), "uncontained".to_string()]
    );
}

#[test]
fn the_contained_lane_matches_the_hand_written_table() {
    check_lane("contained", CONTAINED);
}

#[test]
fn the_uncontained_lane_matches_the_hand_written_table() {
    check_lane("uncontained", UNCONTAINED);
}

#[test]
fn the_uncontained_lane_does_not_claim_the_cgroup_guards() {
    // Named one at a time so a regression says WHICH claim came back.
    let uncontained = lane("uncontained");
    for key in [
        "cpu_timeout",
        "memory_max",
        "oom_detection",
        "cpu_bandwidth",
        "cpu_affinity",
    ] {
        assert_eq!(
            uncontained.get(key).and_then(Value::as_bool),
            Some(false),
            "uncontained.{key} must not be advertised"
        );
    }
}

#[test]
fn the_manifest_is_compact_json_with_no_incidental_whitespace() {
    // The Python twin is a hand-written string literal held byte-identical by `make cross`; a
    // stray space here is a cross-language failure, so catch it in-engine first. serde_json's
    // default map is sorted, so this also pins key order.
    let round_tripped = serde_json::to_string(&manifest()).expect("re-serialize");
    assert_eq!(round_tripped, published());
}

/// The manifest is DERIVED, so the thing worth pinning is that the derivation agrees with what
/// each guard site is told when it asks. Both directions are named literally.
#[test]
fn every_published_flag_is_what_a_guard_site_would_be_told() {
    with_registry_pinned(|| {
        for (lane_name, lane, table) in [
            ("contained", Lane::Contained, CONTAINED),
            ("uncontained", Lane::Uncontained, UNCONTAINED),
        ] {
            for (key, want) in table {
                assert_eq!(
                    is_enforced(key, lane),
                    *want,
                    "{lane_name}.{key}: the manifest and the guard site disagree"
                );
            }
        }
    });
}
