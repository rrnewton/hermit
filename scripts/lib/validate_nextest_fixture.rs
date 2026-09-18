//! Authentic producer/runner/ledger coverage. All generated sources and results
//! remain in the script harness's retained directory, outside product inventory.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use super::super::DagConfig;
use super::super::LedgerCtx;
use super::super::Plan;
use super::super::dag_to_json;
use super::super::finish_committed_selection;
use super::super::invocation_deadline_ns;
use super::super::libtest_counts;
use super::super::run_lane_once;
use super::super::step_with_caps;
use super::super::utc_now;
use super::super::validate_evidence;
use super::super::validate_test_results;
use super::super::write_ledger;
use super::*;

fn git_text(source: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(source)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn quoted(path: &Path) -> String {
    super::super::validate_plan::shell_quote(path.to_str().unwrap())
}

fn context(source: &Path, outcomes: &[StepOutcome]) -> LedgerCtx {
    let (executed_tests, passed_tests, filtered_tests) = libtest_counts(outcomes);
    // An exact clean source is required for cumulative artifact binding. The
    // fixture remains nonqualifying even when its sole passing control passes.
    assert!(
        git_text(
            source,
            &["status", "--porcelain", "--untracked-files=normal"]
        )
        .is_empty()
    );
    LedgerCtx {
        started_at: utc_now(),
        host: "real-nextest-fixture".into(),
        toolchain: "fixture; actual tool versions retained separately".into(),
        slot: "private-script-test-fixture".into(),
        cwd: source.display().to_string(),
        profile: "full".into(),
        selection_mode: "only".into(),
        cache_state: "fixture-owned".into(),
        commit: git_text(source, &["rev-parse", "HEAD"]),
        tree: git_text(source, &["rev-parse", "HEAD^{tree}"]),
        git_depth: 0,
        git_ahead: 0,
        git_behind: 0,
        commit_anchored: false,
        tree_dirty: false,
        dag_jobs: 1,
        admission: None,
        base_sha: serde_json::Value::Null,
        base_tree: serde_json::Value::Null,
        reverie_base_sha: serde_json::Value::Null,
        reverie_base_tree: serde_json::Value::Null,
        reverie_pin_current: false,
        concurrent_validates: None,
        concurrency_proof: None,
        interruption: None,
        // These serializer fixture fields are not a whole-run CPU measurement.
        // Real per-attempt CPU records are retained and checked below.
        cpu_user: 0.0,
        cpu_sys: 0.0,
        retry_rounds: 0,
        executed_tests,
        passed_tests,
        filtered_tests,
    }
}

fn expected_ids(diagnostics: &Path, names: &BTreeSet<String>) -> BTreeMap<String, bool> {
    let inventory = json(&diagnostics.join("inventory.json"));
    let suites = inventory["rust-suites"].as_object().unwrap();
    assert_eq!(
        suites.len(),
        1,
        "only the private fixture library is selected"
    );
    let suite = suites.values().next().unwrap();
    assert_eq!(suite["binary-name"], "hermit_structured_results_fixture");
    assert_eq!(suite["kind"], "lib");
    let selected: BTreeSet<String> = suite["testcases"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, value)| value["filter-match"]["status"] == "matches")
        .map(|(name, _)| name.clone())
        .collect();
    assert_eq!(
        &selected, names,
        "actual Nextest inventory must equal the requested tests"
    );
    let package = suite["package-name"].as_str().unwrap();
    let mut actual = BTreeMap::new();
    for line in fs::read_to_string(diagnostics.join("events.jsonl"))
        .unwrap()
        .lines()
    {
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        if event["type"] != "test" {
            continue;
        }
        let passed = match event["event"].as_str() {
            Some("ok") => true,
            Some("failed") => false,
            _ => continue,
        };
        let name = event["name"].as_str().unwrap();
        let (event_package, binary_test) = name.split_once("::").unwrap();
        assert_eq!(event_package, package);
        let (_, test) = binary_test.split_once('$').unwrap();
        assert!(names.contains(test), "unexpected terminal event {event}");
        assert!(
            actual.insert(format!("{package}${test}"), passed).is_none(),
            "duplicate terminal event"
        );
    }
    assert_eq!(actual.len(), names.len());
    for name in names {
        assert_eq!(actual[&format!("{package}${name}")], name == "passes");
    }
    let records =
        hermit_manifest_plan::nextest_cpu::read_attempt_records(&diagnostics.join("attempts"))
            .unwrap();
    assert_eq!(
        records.len(),
        names.len(),
        "each actual test needs one CPU record"
    );
    let mut measured = BTreeSet::new();
    for record in records {
        assert_eq!(record.identity.package, package);
        assert_eq!(record.identity.attempt, 1);
        assert!(measured.insert(record.identity.test.clone()));
        assert!(names.contains(&record.identity.test));
        // The production wrapper decides its accounting source. Do not assert
        // that its ordinary wait4/procfs path is cgroup CPU enforcement.
        let encoded = serde_json::to_value(&record).unwrap();
        assert!(
            encoded["cpu_source"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert_eq!(encoded["completion"]["kind"], "exit");
        let code = encoded["completion"]["code"].as_i64().unwrap();
        assert_eq!(code == 0, record.identity.test == "passes");
    }
    assert_eq!(&measured, names);
    actual
}

#[test]
fn actual_nextest_results_and_publication_failures() {
    let source = Path::new(file!())
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let parent = PathBuf::from(
        std::env::var_os("DAGRUN_LOG_DIR")
            .expect("run this test through ci/rust-script-bin/run-test-harness"),
    );
    let root = tempfile::Builder::new()
        .prefix("real-nextest-results-")
        .tempdir_in(parent)
        .unwrap()
        .keep();
    eprintln!("authentic Nextest fixture evidence: {}", root.display());
    assert!(
        git_text(
            &source,
            &["status", "--porcelain", "--untracked-files=normal"]
        )
        .is_empty(),
        "commit the candidate before running the exact-source fixture"
    );
    let original_head = git_text(&source, &["rev-parse", "HEAD"]);
    let fixture = root.join("crate");
    fs::create_dir_all(fixture.join("src")).unwrap();
    fs::write(fixture.join("Cargo.toml"), concat!(
        "[package]\nname = \"hermit\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n",
        "[lib]\nname = \"hermit_structured_results_fixture\"\n[workspace]\n",
    )).unwrap();
    fs::write(fixture.join("src/lib.rs"), concat!(
        "#[test]\nfn passes() { assert_eq!(2 + 2, 4); }\n",
        "#[test]\nfn fails() { assert_eq!(2 + 2, 5, \"intentional structured-result fixture assertion\"); }\n",
    )).unwrap();
    let target = root.join("target");
    // This nested fixture spends the enclosing scheduler step's clock. It must
    // not start a fresh allowance after the harness has already consumed time.
    let deadline = invocation_deadline_ns(Some(1200), true)
        .expect("the fixture needs its enclosing scheduler's monotonic start")
        .unwrap();
    let cargo_home = PathBuf::from(
        std::env::var_os("CARGO_HOME")
            .expect("the bounded launcher must name its owned CARGO_HOME"),
    )
    .canonicalize()
    .unwrap();
    fs::write(root.join("bounds.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "enclosing_step_allowance_seconds":1200,"deadline_monotonic_ns":deadline,
        "per_case_wall_seconds":180,"per_case_cpu_seconds":300,
        "declared_memory_bytes":2_u64*1024*1024*1024,"dag_width":1,"cargo_jobs":1,"nextest_retries":0,
        "per_case_cgroup_binding":null,"outer_scope_required":true,"cargo_home":cargo_home,
        "source":source,"source_head":original_head,"private_target":target,
        "metadata_root":source.join("target/ci/nextest-binaries"),
        "evidence":root,"diagnostic_only":true,"admission":null,
    })).unwrap()).unwrap();
    for (index, (name, failing, deny_publish, mismatch, expected_class)) in [
        (
            "writable-pass",
            false,
            false,
            false,
            NodeClassification::Pass,
        ),
        (
            "writable-failure",
            true,
            false,
            false,
            NodeClassification::ProductFailure,
        ),
        (
            "publish-after-pass",
            false,
            true,
            false,
            NodeClassification::NoResult,
        ),
        (
            "publish-after-failure",
            true,
            true,
            false,
            NodeClassification::ProductFailure,
        ),
        (
            "count-before-publish",
            false,
            true,
            true,
            NodeClassification::ProductFailure,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let case = root.join(name);
        fs::create_dir_all(case.join("tmp")).unwrap();
        let diagnostics = case.join("diagnostics"); // Retainer must create this exclusively.
        let names: BTreeSet<String> = if failing {
            ["passes", "fails"].into_iter().map(str::to_owned).collect()
        } else {
            BTreeSet::from(["passes".into()])
        };
        let filter = if failing {
            "test(=passes) | test(=fails)"
        } else {
            "test(=passes)"
        };
        let expected = if failing || mismatch { 2 } else { 1 };
        let lock = if index == 0 {
            format!(
                "cargo generate-lockfile --offline --manifest-path {} || exit $?; cargo --version; cargo nextest --version; rustc --version; ",
                quoted(&fixture.join("Cargo.toml")),
            )
        } else {
            String::new()
        };
        let fault = if deny_publish {
            "mkdir -- \"$DAGRUN_TEST_COUNTS_PATH\" || exit $?; "
        } else {
            ""
        };
        let command = format!(
            "cd {} || exit $?; export PATH={}:\"$PATH\"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT={}; \
             export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1 CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=1; \
             export CARGO_TARGET_DIR={} TMPDIR={}; unset HERMIT_PREPARED_NEXTEST_REQUIRED; \
             printf '%s\\n' \"$DAGRUN_TEST_COUNTS_PATH\" > {}; {lock}{fault}set +e; \
             NEXTEST_EXPECTED_EXECUTED={expected} HERMIT_NEXTEST_CPU_REPORT_PATH={} \
             HERMIT_NEXTEST_RETAIN_DIAGNOSTICS_DIR={} {} --manifest-path {} --locked --offline \
             --profile ci --lib -j 1 --retries 0 --no-tests fail -E {} >{} 2>{}; \
             fixture_status=$?; cat {}; cat {} >&2; exit \"$fixture_status\"",
            quoted(&source),
            quoted(&source.join("ci/rust-script-bin")),
            quoted(&source.join("target/ci/rust-scripts")),
            quoted(&target),
            quoted(&case.join("tmp")),
            quoted(&case.join("result-path.txt")),
            quoted(&case.join("cpu.json")),
            quoted(&diagnostics),
            quoted(&source.join("ci/run-nextest-counted.sh")),
            quoted(&fixture.join("Cargo.toml")),
            super::super::validate_plan::shell_quote(filter),
            quoted(&case.join("producer.stdout")),
            quoted(&case.join("producer.stderr")),
            quoted(&case.join("producer.stdout")),
            quoted(&case.join("producer.stderr")),
        );
        let mut step = step_with_caps(
            "fixture",
            name,
            "real Nextest structured-results fixture",
            command,
            Vec::new(),
            180,
            300,
            2 * 1024 * 1024 * 1024,
        );
        step.result_manifests = Some(vec![dagrun::model::ResultManifest::StructuredTestResults(
            dagrun::model::StructuredTestResultsManifest::current(step.tag()),
        )]);
        let cfg = DagConfig {
            steps: vec![step],
            ..Default::default()
        };
        fs::create_dir_all(case.join("ci/dag")).unwrap();
        let dag = dag_to_json(&cfg);
        let dag_path = case.join("ci/dag/validate.json");
        fs::write(&dag_path, &dag).unwrap();
        fs::write(
            case.join("ci/expected-e2e-plan.json"),
            "{\"schema\":1,\"cells\":[]}",
        )
        .unwrap();
        let plan = finish_committed_selection(
            Plan {
                cfg,
                profile: "full".into(),
                ..Plan::default()
            },
            dag_path,
            dag.into_bytes(),
        );
        let run_id = format!("{}-{name}", root.file_name().unwrap().to_str().unwrap());
        let prepared = validate_evidence::SelectedEvidence::capture(&case, &plan)
            .unwrap()
            .publish(&case, &plan, &run_id, &original_head)
            .unwrap();
        let started = Instant::now();
        let result = run_lane_once(
            &plan.cfg,
            1,
            true,
            0,
            None,
            &case.join("driver.log"),
            Some(deadline),
            false,
        );
        assert!(
            result.complete && !result.run_timed_out,
            "{name}: incomplete fixture; evidence {}",
            case.display()
        );
        assert_eq!(result.outcomes.len(), 1);
        assert_eq!(result.attempts.len(), 1, "no outer retry");
        let outcome = &result.outcomes[0];
        assert!(!outcome.aborted && !outcome.timed_out && !outcome.cpu_timed_out && !outcome.oomed);
        assert_eq!(outcome.oom_kills, 0);
        let capture = json(&diagnostics.join("capture.json"));
        assert_eq!(capture["capture_complete"], true, "{capture}");
        let nextest_status = if failing { 100 } else { 0 };
        let writer_status = if mismatch {
            2
        } else if deny_publish {
            75
        } else {
            0
        };
        let wrapper_status = if failing {
            nextest_status
        } else {
            writer_status
        };
        assert_eq!(capture["nextest_status"], nextest_status);
        assert_eq!(capture["writer_status"], writer_status);
        assert_eq!(capture["wrapper_status"], wrapper_status);
        assert_eq!(outcome.returncode, Some(wrapper_status));
        let actual = expected_ids(&diagnostics, &names);
        assert_eq!(
            node_classification(outcome, &result.attempts),
            expected_class
        );
        if deny_publish {
            let path = PathBuf::from(
                fs::read_to_string(case.join("result-path.txt"))
                    .unwrap()
                    .trim(),
            );
            assert!(
                path.is_dir(),
                "the actual scheduler-injected destination was made a directory"
            );
            assert_eq!(
                outcome.test_results_error_kind,
                Some(dagrun::TestResultsErrorKind::ReadIo)
            );
            assert!(
                outcome.test_results.is_none()
                    && outcome.executed_tests.is_none()
                    && outcome.filtered_tests.is_none()
            );
            let stderr = fs::read_to_string(case.join("producer.stderr")).unwrap();
            if mismatch {
                assert!(stderr.contains(
                    "expected 2 tests to execute, saw 1, of which 1 passed and 0 failed"
                ));
                assert!(!stderr.contains("structured-test-results-publish"));
            } else {
                assert!(stderr.contains("structured-test-results-publish"));
            }
            assert!(
                !case.join("cpu.json").exists(),
                "primary report refusal still precedes aggregate CPU publication"
            );
            fs::remove_dir(path).unwrap(); // Only our own empty fault fixture, after import.
        } else {
            assert!(
                outcome.test_results_error.is_none() && outcome.test_results_error_kind.is_none()
            );
            let rows: BTreeMap<String, bool> = outcome
                .test_results
                .as_ref()
                .unwrap()
                .iter()
                .map(|row| (row.id.clone(), row.passed))
                .collect();
            assert_eq!(rows, actual);
            assert_eq!(outcome.executed_tests, Some(names.len() as u64));
            assert_eq!(
                json(&case.join("cpu.json"))["attempts"]
                    .as_array()
                    .unwrap()
                    .len(),
                names.len()
            );
        }
        let ctx = context(&source, &result.outcomes);
        let retained =
            prepared.retain(&case, &case, &ctx, &result.outcomes, &result.attempts, None);
        let retained = if deny_publish {
            assert!(
                retained.is_err(),
                "unknown test rows must not acquire a cumulative artifact"
            );
            None
        } else {
            Some(retained.unwrap())
        };
        let ledger = case.join("fixture-ledger.jsonl");
        let planned = result
            .outcomes
            .iter()
            .map(|outcome| outcome.tag.clone())
            .collect();
        let exit = match expected_class {
            NodeClassification::Pass => 0,
            NodeClassification::NoResult => 75,
            _ => 1,
        };
        write_ledger(
            &ledger,
            &ctx,
            &result.outcomes,
            &result.attempts,
            &[],
            &[],
            &planned,
            started.elapsed().as_secs_f64(),
            exit,
            case.join("producer.stderr").to_str().unwrap(),
            result.complete,
            serde_json::json!({"run_id":run_id}),
            None,
            retained.as_ref(),
        );
        let text = fs::read_to_string(&ledger).unwrap();
        assert_eq!(text.lines().count(), 1);
        let row: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(row["result"], expected_class.result());
        assert_eq!(row["selection_mode"], "only");
        assert_eq!(row["commit_anchored"], false);
        assert!(row["admission"].is_null());
        if let Some(retained) = retained {
            retained
                .verify_record(&serde_json::from_value(row.clone()).unwrap())
                .unwrap();
            let artifact: hermit_manifest_plan::ledger::TestResultsEvidenceV9 =
                serde_json::from_value(row["test_results"].clone()).unwrap();
            let bytes = fs::read(case.join(&artifact.artifact.path)).unwrap();
            let rows = validate_test_results::verify_artifact(&artifact, &bytes).unwrap();
            let observed: BTreeMap<String, bool> = rows
                .into_iter()
                .map(|item| {
                    assert_eq!(item.run_id, run_id);
                    assert_eq!(item.hermit_sha, original_head);
                    (
                        item.id,
                        item.result == hermit_manifest_plan::ledger::TestResultVerdict::Pass,
                    )
                })
                .collect();
            assert_eq!(
                observed, actual,
                "authentic failing ID must reach the retained ledger artifact"
            );
        } else {
            assert!(row["executed_tests"].is_null() && row["passed_tests"].is_null());
            assert!(row.get("test_results").is_none());
            assert_eq!(row["gates"][0]["test_results_error_kind"], "read_io");
            assert_eq!(
                row["gates"][0]["attempts"][0]["test_results_error_kind"],
                "read_io"
            );
        }
        fs::write(
            case.join("checked.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "actual_nextest_status":nextest_status,"actual_writer_status":writer_status,
                "actual_wrapper_status":wrapper_status,"classification":expected_class.as_str(),
                "test_ids_from_real_events":actual,"wall_seconds":started.elapsed().as_secs_f64(),
            }))
            .unwrap(),
        )
        .unwrap();
    }
    assert_eq!(git_text(&source, &["rev-parse", "HEAD"]), original_head);
    assert!(
        git_text(
            &source,
            &["status", "--porcelain", "--untracked-files=normal"]
        )
        .is_empty()
    );
}
