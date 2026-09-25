/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Pin the fold of the backend-parity-c bucket into c-programs.
//!
//! Slice S6 of <https://github.com/rrnewton/hermit/issues/3301> deleted
//! `tests/e2e/manifests/backend-parity-c.yaml`, moved its 104 tests into
//! `tests/e2e/manifests/c-programs.yaml` and its fixtures into `tests/c`, and
//! recorded every old id in `tests/e2e/manifests/inventory/retired-ids.json`.
//! The fold is a rename, not a change of coverage. This file checks that from
//! four independent sides:
//!
//! 1. The retired-id map renames exactly the documented ids: every id is the
//!    bucket-prefix rename except the one collision, and it is a bijection onto
//!    live ids.
//! 2. The committed CI plan selects the pre-fold plan's 859 cells, 855
//!    portable and 4 privileged, with per-(lane, backend, mode) counts equal to
//!    the pre-fold plan's, plus exactly the cells `PLAN_CELLS_ADDED_AFTER_FOLD`
//!    names.
//! 3. The committed compatibility cell table holds the pre-fold table's 5776
//!    rows with per-(backend, mode, status) counts equal to the pre-fold
//!    table's, plus the rows of each test `TESTS_ADDED_AFTER_FOLD` names; a row
//!    `CELL_STATUS_CHANGES_AFTER_FOLD` names counts under its pre-fold status.
//! 4. The command the c-programs nodes run refuses a selection of zero cells,
//!    so folding more tests into that node cannot turn it into a vacuous pass.
//!
//! The pre-fold numbers are embedded, not recomputed: they were measured at
//! e8007f971a72c5fd92fcf3e17e41f38811c3cac3. The fold's parent on main is
//! 3f66a249b30fada86b81e722b8e5439ac0789f8e, the last commit that declared
//! backend-parity-c; the plan, the cell table and both manifests are
//! byte-identical at the two commits. A later change that moves a count has to
//! change this file and say why.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use hermit_manifest_plan::retired_ids::RetiredIds;
use serde_json::Value as JsonValue;
use serde_yaml::Value;

const RETIRED_BUCKET: &str = "backend-parity-c";
const SUCCESSOR_BUCKET: &str = "c-programs";
const LAST_LIVE_COMMIT: &str = "3f66a249b30fada86b81e722b8e5439ac0789f8e";
const FOLDED_TESTS: usize = 104;

/// Every successor that is not the plain prefix rename, and why. c-programs
/// already declared `c-programs/pidfd-open-self` for `tests/c/pidfd_open_self.c`,
/// a different program from the backend-parity-c fixture of the same name, so
/// the moved test and its fixture took a `-pair` suffix instead of overwriting.
const COLLISION_RENAMES: &[(&str, &str)] = &[(
    "backend-parity-c/pidfd-open-self",
    "c-programs/pidfd-open-self-pair",
)];

/// Pre-fold `ci/expected-e2e-plan.json` cells per (lane, backend, mode).
const PLAN_COUNTS: &[(&str, &str, &str, usize)] = &[
    ("portable", "kvm", "verify", 242),
    ("portable", "liteinst", "custom", 1),
    ("portable", "liteinst", "verify", 145),
    ("portable", "ptrace", "chaos", 6),
    ("portable", "ptrace", "custom", 2),
    ("portable", "ptrace", "replay", 2),
    ("portable", "ptrace", "verify", 345),
    ("portable", "sabre", "verify", 112),
    ("privileged", "kvm", "verify", 2),
    ("privileged", "liteinst", "verify", 1),
    ("privileged", "ptrace", "verify", 1),
];

/// Pre-fold `ci/compat-envelope/cells.json` rows per (backend, mode, status).
const CELL_COUNTS: &[(&str, &str, &str, usize)] = &[
    ("dbt", "chaos", "not-applicable", 361),
    ("dbt", "replay", "not-applicable", 361),
    ("dbt", "verify", "not-applicable", 300),
    ("dbt", "verify", "red", 61),
    ("kvm", "chaos", "not-applicable", 361),
    ("kvm", "replay", "not-applicable", 361),
    ("kvm", "verify", "green", 244),
    ("kvm", "verify", "not-applicable", 109),
    ("kvm", "verify", "red", 8),
    ("liteinst", "chaos", "not-applicable", 361),
    ("liteinst", "replay", "not-applicable", 361),
    ("liteinst", "verify", "green", 146),
    ("liteinst", "verify", "not-applicable", 212),
    ("liteinst", "verify", "red", 3),
    ("native", "naked", "not-applicable", 328),
    ("native", "naked", "red", 33),
    ("ptrace", "chaos", "green", 6),
    ("ptrace", "chaos", "not-applicable", 354),
    ("ptrace", "chaos", "red", 1),
    ("ptrace", "replay", "green", 2),
    ("ptrace", "replay", "not-applicable", 359),
    ("ptrace", "verify", "green", 346),
    ("ptrace", "verify", "not-applicable", 3),
    ("ptrace", "verify", "red", 12),
    ("sabre", "chaos", "not-applicable", 361),
    ("sabre", "replay", "not-applicable", 361),
    ("sabre", "verify", "green", 112),
    ("sabre", "verify", "not-applicable", 217),
    ("sabre", "verify", "red", 32),
];

/// Cells the committed plan selects that the pre-fold plan did not, as
/// (lane, backend, mode, test), each under the change that selected it.
/// Every other committed cell is counted against `PLAN_COUNTS`.
const PLAN_CELLS_ADDED_AFTER_FOLD: &[(&str, &str, &str, &str)] = &[
    // https://github.com/rrnewton/hermit/pull/3224 promotes this existing test.
    (
        "portable",
        "ptrace",
        "verify",
        "c-programs/dbt-pid-virtualization",
    ),
    // https://github.com/rrnewton/hermit/pull/3224 adds this test.
    (
        "portable",
        "ptrace",
        "verify",
        "c-programs/sigsuspend-alarm-wake",
    ),
];

/// Tests the committed cell table lists that the pre-fold table did not, as
/// (test, backend, mode, status) of the one row that is not `not-applicable`.
const TESTS_ADDED_AFTER_FOLD: &[(&str, &str, &str, &str)] = &[
    // https://github.com/rrnewton/hermit/pull/3224
    (
        "c-programs/sigsuspend-alarm-wake",
        "ptrace",
        "verify",
        "green",
    ),
];

/// Pre-fold rows whose status changed after the fold, as (test, backend,
/// mode, pre-fold status, committed status).
const CELL_STATUS_CHANGES_AFTER_FOLD: &[(&str, &str, &str, &str, &str)] = &[
    // https://github.com/rrnewton/hermit/pull/3224
    (
        "c-programs/dbt-pid-virtualization",
        "ptrace",
        "verify",
        "red",
        "green",
    ),
];

/// The cell table has one row per test for each of these 16 (backend, mode)
/// pairs: dbt, kvm, liteinst, ptrace and sabre each in verify, replay and
/// chaos, and native in naked.
const ROWS_PER_TEST: usize = 16;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("ci/manifest-plan must sit two levels below the repository root")
        .to_path_buf()
}

fn read_json(path: &str) -> JsonValue {
    let path = repo_root().join(path);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()))
}

fn field<'a>(row: &'a JsonValue, name: &str) -> &'a str {
    row.get(name)
        .and_then(JsonValue::as_str)
        .unwrap_or_else(|| panic!("row has no string {name}: {row}"))
}

/// Every manifest bucket and, per test id, its program, read from the YAML
/// the harness reads.
struct Manifests {
    buckets: BTreeSet<String>,
    programs: BTreeMap<String, Option<String>>,
}

fn manifests() -> Manifests {
    let dir = repo_root().join("tests/e2e/manifests");
    let mut buckets = BTreeSet::new();
    let mut programs = BTreeMap::new();
    let mut entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("cannot list {}: {error}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        let document: Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let Some(bucket) = document.get("bucket").and_then(Value::as_str) else {
            continue;
        };
        assert!(buckets.insert(bucket.to_string()), "bucket {bucket} twice");
        for test in document
            .get("test")
            .and_then(Value::as_sequence)
            .unwrap_or_else(|| panic!("{} has no test list", path.display()))
        {
            let id = test.get("id").and_then(Value::as_str).unwrap().to_string();
            let program = test
                .get("program")
                .and_then(Value::as_str)
                .map(str::to_string);
            assert!(programs.insert(id.clone(), program).is_none(), "{id} twice");
        }
    }
    assert!(!buckets.is_empty() && !programs.is_empty());
    Manifests { buckets, programs }
}

fn retired_ids() -> RetiredIds {
    RetiredIds::load(&repo_root()).unwrap()
}

#[test]
fn the_retired_id_map_is_the_prefix_rename_plus_the_documented_collision() {
    let map = retired_ids();
    assert_eq!(map.retirements.len(), 1, "only backend-parity-c is retired");
    let retirement = map.retirement(RETIRED_BUCKET).unwrap();
    assert_eq!(retirement.successor_bucket, SUCCESSOR_BUCKET);
    assert_eq!(retirement.last_live_commit, LAST_LIVE_COMMIT);
    assert_eq!(retirement.ids.len(), FOLDED_TESTS);
    let collisions = COLLISION_RENAMES
        .iter()
        .map(|(old, new)| (old.to_string(), new.to_string()))
        .collect::<BTreeMap<_, _>>();
    let mut renamed = BTreeMap::new();
    for (old, new) in &retirement.ids {
        let name = old
            .strip_prefix("backend-parity-c/")
            .unwrap_or_else(|| panic!("{old} is not a backend-parity-c id"));
        if format!("c-programs/{name}") != *new {
            renamed.insert(old.clone(), new.clone());
        }
    }
    assert_eq!(
        renamed, collisions,
        "only the documented collision may differ from the prefix rename"
    );
    for (old, new) in COLLISION_RENAMES {
        assert!(
            retirement.reason.contains(old) && retirement.reason.contains(new),
            "the retirement reason must document {old} -> {new}"
        );
        assert!(
            retirement.collisions[*old].contains(new),
            "the listed collision {old} must name its successor {new}"
        );
    }
    assert_eq!(
        retirement.collisions.keys().collect::<BTreeSet<_>>(),
        collisions.keys().collect::<BTreeSet<_>>(),
        "the production map lists exactly the documented collisions"
    );
}

/// The rename rule is enforced by the production parser, not only by the test
/// above: a retired id pointed at an unrelated live test is refused even though
/// that test is live and no other retired id maps to it, and the one real
/// collision is refused if it is not listed.
#[test]
fn the_production_parser_refuses_a_retired_id_mapped_onto_an_unrelated_live_test() {
    let path = repo_root().join(hermit_manifest_plan::retired_ids::RETIRED_IDS_FILE);
    let committed = std::fs::read_to_string(&path).unwrap();
    RetiredIds::parse(&committed).unwrap();

    let manifests = manifests();
    let unrelated = "c-programs/add-key-enosys";
    assert!(
        manifests.programs.contains_key(unrelated),
        "{unrelated} is live"
    );
    assert!(
        retired_ids()
            .retirement(RETIRED_BUCKET)
            .unwrap()
            .ids
            .values()
            .all(|new| new != unrelated),
        "{unrelated} is not already a successor, so only the rename rule can refuse it"
    );
    let rename = "\"backend-parity-c/aio-refusal\": \"c-programs/aio-refusal\"";
    assert_eq!(committed.matches(rename).count(), 1);
    let misdirected = committed.replace(
        rename,
        &format!("\"backend-parity-c/aio-refusal\": \"{unrelated}\""),
    );
    let error = RetiredIds::parse(&misdirected).unwrap_err();
    assert!(
        error.contains(
            "successor \"c-programs/add-key-enosys\" of \"backend-parity-c/aio-refusal\" is neither the prefix rename c-programs/aio-refusal nor a listed collision"
        ),
        "{error}"
    );

    let mut unlisted: JsonValue = serde_json::from_str(&committed).unwrap();
    let removed = unlisted["retirements"][0]["collisions"]
        .as_object_mut()
        .unwrap()
        .remove(COLLISION_RENAMES[0].0);
    assert!(removed.is_some());
    let error = RetiredIds::parse(&unlisted.to_string()).unwrap_err();
    assert!(
        error.contains(
            "is neither the prefix rename c-programs/pidfd-open-self nor a listed collision"
        ),
        "{error}"
    );
}

#[test]
fn the_retired_id_map_is_a_bijection_onto_live_ids() {
    let map = retired_ids();
    let manifests = manifests();
    let live = manifests.programs.keys().cloned().collect::<BTreeSet<_>>();
    map.check_live(&live, &manifests.buckets).unwrap();
    let retirement = map.retirement(RETIRED_BUCKET).unwrap();
    let successors = retirement.ids.values().collect::<BTreeSet<_>>();
    assert_eq!(successors.len(), FOLDED_TESTS, "no successor is shared");
    assert!(!manifests.buckets.contains(RETIRED_BUCKET));
    assert!(
        !repo_root()
            .join("tests/e2e/manifests/backend-parity-c.yaml")
            .exists()
    );
    assert!(
        live.iter().all(|id| !id.starts_with("backend-parity-c/")),
        "a backend-parity-c id is still declared"
    );
    // Every successor is a live c-programs test whose program sits in tests/c.
    // The collision kept both programs: the pre-existing test still names its
    // own source, and the moved one names the renamed copy.
    for new in &successors {
        let program = manifests.programs[new.as_str()]
            .as_deref()
            .unwrap_or_else(|| panic!("{new} has no program"));
        assert!(program.starts_with("tests/c/"), "{new} runs {program}");
        assert!(repo_root().join(program).is_file(), "{program} is missing");
    }
    assert_eq!(
        manifests.programs["c-programs/pidfd-open-self"].as_deref(),
        Some("tests/c/pidfd_open_self.c")
    );
    assert_eq!(
        manifests.programs["c-programs/pidfd-open-self-pair"].as_deref(),
        Some("tests/c/pidfd_open_self_pair.c")
    );
    // No two tests of the merged bucket may name one program: the fold must
    // not have pointed a moved test at an unrelated c-programs source.
    let mut owners = BTreeMap::<&str, Vec<&str>>::new();
    for (id, program) in &manifests.programs {
        if let (true, Some(program)) = (id.starts_with("c-programs/"), program) {
            owners.entry(program).or_default().push(id);
        }
    }
    for new in &successors {
        let program = manifests.programs[new.as_str()].as_deref().unwrap();
        assert_eq!(owners[program], vec![new.as_str()], "{program} is shared");
    }
}

/// Whether `cell` is the plan cell (lane, backend, mode, test).
fn is_cell(cell: &JsonValue, lane: &str, backend: &str, mode: &str, test: &str) -> bool {
    field(cell, "lane") == lane
        && field(cell, "backend") == backend
        && field(cell, "mode") == mode
        && field(cell, "test") == test
}

/// Whether `cell` is one of `PLAN_CELLS_ADDED_AFTER_FOLD`.
fn added_after_fold(cell: &JsonValue) -> bool {
    PLAN_CELLS_ADDED_AFTER_FOLD
        .iter()
        .any(|&(lane, backend, mode, test)| is_cell(cell, lane, backend, mode, test))
}

/// Whether `row` is the cell-table row of (test, backend, mode).
fn is_row(row: &JsonValue, test: &str, backend: &str, mode: &str) -> bool {
    field(row, "test") == test && field(row, "backend") == backend && field(row, "mode") == mode
}

#[test]
fn the_committed_plan_keeps_its_cell_counts() {
    let plan = read_json("ci/expected-e2e-plan.json");
    let cells = plan["cells"].as_array().unwrap();
    let lane = |name: &str| cells.iter().filter(|c| field(c, "lane") == name).count();
    assert_eq!(
        (cells.len(), lane("portable"), lane("privileged")),
        (861, 857, 4)
    );
    for &(added_lane, backend, mode, test) in PLAN_CELLS_ADDED_AFTER_FOLD {
        let selected = cells
            .iter()
            .filter(|cell| is_cell(cell, added_lane, backend, mode, test))
            .count();
        assert_eq!(selected, 1, "{added_lane} {backend} {mode} {test}");
    }
    let pre_fold = cells
        .iter()
        .filter(|cell| !added_after_fold(cell))
        .collect::<Vec<_>>();
    let pre_fold_lane = |name: &str| pre_fold.iter().filter(|c| field(c, "lane") == name).count();
    assert_eq!(
        (
            pre_fold.len(),
            pre_fold_lane("portable"),
            pre_fold_lane("privileged")
        ),
        (859, 855, 4)
    );
    let mut counts = BTreeMap::<(String, String, String), usize>::new();
    for cell in &pre_fold {
        let key = (
            field(cell, "lane").to_string(),
            field(cell, "backend").to_string(),
            field(cell, "mode").to_string(),
        );
        *counts.entry(key).or_default() += 1;
    }
    let expected = PLAN_COUNTS
        .iter()
        .map(|&(lane, backend, mode, n)| ((lane.into(), backend.into(), mode.into()), n))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(counts, expected);
    // The folded cells now belong to c-programs: 437 portable c-programs cells
    // and 276 portable plus 3 privileged backend-parity-c cells before the fold.
    // A cell added after the fold is not counted here.
    let retirement = retired_ids();
    let successors = retirement.successors_of(RETIRED_BUCKET).unwrap();
    let mut by_bucket = BTreeMap::<(String, String), usize>::new();
    for cell in cells {
        let category = field(cell, "category");
        assert_ne!(category, RETIRED_BUCKET, "{cell}");
        if successors.contains(field(cell, "test")) {
            assert_eq!(category, SUCCESSOR_BUCKET, "{cell}");
        }
        if category == SUCCESSOR_BUCKET && !added_after_fold(cell) {
            *by_bucket
                .entry((field(cell, "lane").into(), category.into()))
                .or_default() += 1;
        }
    }
    assert_eq!(
        by_bucket,
        BTreeMap::from([
            (("portable".into(), "c-programs".into()), 437 + 276),
            (("privileged".into(), "c-programs".into()), 3),
        ])
    );
}

#[test]
fn the_committed_cell_table_keeps_its_row_counts() {
    let table = read_json("ci/compat-envelope/cells.json");
    let rows = table["cells"].as_array().unwrap();
    assert_eq!(rows.len(), 5792);
    let mut added_rows = 0;
    for &(test, backend, mode, status) in TESTS_ADDED_AFTER_FOLD {
        let own = rows
            .iter()
            .filter(|row| field(row, "test") == test)
            .collect::<Vec<_>>();
        assert_eq!(own.len(), ROWS_PER_TEST, "{test}");
        let applicable = own
            .iter()
            .filter(|row| field(row, "status") != "not-applicable")
            .collect::<Vec<_>>();
        assert_eq!(applicable.len(), 1, "{test}");
        assert!(is_row(applicable[0], test, backend, mode), "{test}");
        assert_eq!(field(applicable[0], "status"), status, "{test}");
        added_rows += own.len();
    }
    assert_eq!(rows.len() - added_rows, 5776);
    for &(test, backend, mode, _, status) in CELL_STATUS_CHANGES_AFTER_FOLD {
        let matching = rows
            .iter()
            .filter(|row| is_row(row, test, backend, mode))
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "{test} {backend} {mode}");
        assert_eq!(
            field(matching[0], "status"),
            status,
            "{test} {backend} {mode}"
        );
    }
    let mut counts = BTreeMap::<(String, String, String), usize>::new();
    for row in rows {
        assert_ne!(field(row, "category"), RETIRED_BUCKET, "{row}");
        assert!(
            !field(row, "test").starts_with("backend-parity-c/"),
            "{row}"
        );
        if TESTS_ADDED_AFTER_FOLD
            .iter()
            .any(|&(test, ..)| field(row, "test") == test)
        {
            continue;
        }
        let status = match CELL_STATUS_CHANGES_AFTER_FOLD
            .iter()
            .find(|&&(test, backend, mode, ..)| is_row(row, test, backend, mode))
        {
            Some(&(.., pre_fold, _)) => pre_fold,
            None => field(row, "status"),
        };
        let key = (
            field(row, "backend").to_string(),
            field(row, "mode").to_string(),
            status.to_string(),
        );
        *counts.entry(key).or_default() += 1;
    }
    let expected = CELL_COUNTS
        .iter()
        .map(|&(backend, mode, status, n)| ((backend.into(), mode.into(), status.into()), n))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(counts, expected);
}

/// Run `test-harness` from the repository root and return `(code, stderr)`.
fn harness(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_test-harness"))
        .current_dir(repo_root())
        .args(args)
        .output()
        .expect("failed to run test-harness");
    let code = out
        .status
        .code()
        .unwrap_or_else(|| panic!("test-harness died on a signal for {args:?}"));
    (code, String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn the_c_programs_selector_refuses_zero_cells() {
    // The flags are the ones the DAG generator writes into both c-programs
    // nodes, so this is the node's own selector, not a hand-copied spelling.
    let flags = hermit_manifest_plan::validation_dag::manifest_selector_flags(SUCCESSOR_BUCKET);
    assert!(!flags.contains("--allow-empty"), "{flags}");
    let results = std::env::temp_dir().join(format!(
        "backend-parity-c-fold-empty-{}.jsonl",
        std::process::id()
    ));
    let results = results.to_str().unwrap();
    // A portable test asked for in the privileged lane selects nothing.
    let mut args = vec![
        "run",
        "--lane",
        "privileged",
        "--category",
        SUCCESSOR_BUCKET,
        "--test",
        "c-programs/fork-exec-pipeline",
    ];
    args.extend(flags.split_whitespace());
    args.extend(["--results", results]);
    let (code, stderr) = harness(&args);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("filters selected no cells"),
        "the refusal must be the empty-selection one: {stderr}"
    );
    assert!(!Path::new(results).exists(), "a refused run wrote results");
}
