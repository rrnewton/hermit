use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value;

struct Consumer {
    path: &'static str,
    requirement: &'static str,
    invocation: &'static str,
    minimum_invocations: usize,
}

const CONSUMERS: &[Consumer] = &[
    Consumer {
        path: "tests/backend-parity/e9patch_corpus.py",
        requirement: "matched",
        invocation: "verification_matched(hermit,",
        minimum_invocations: 2,
    },
    Consumer {
        path: "tests/backend-parity/run_matrix.py",
        requirement: "matched",
        invocation: "[str(VERIFICATION_REPORT_BIN), \"matched\", str(path)]",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/e2e/lib/data-handling/common.bash",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/e2e/lib/determinism-stress/common.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/e2e/lib/language-runtimes/run.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/e2e/lib/system-utils/_common.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$VERIFY_REPORT\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/qemu-boot/strict_l2_network_test.sh",
        requirement: "canonical-match",
        invocation: "\"$VERIFICATION_REPORT_BIN\" canonical-match \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/qemu-boot/strict_l2_test.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/qemu-boot/strict_l2_userspace_test.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/standalone/strict_setitimer.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/standalone/strict_timer_create.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn temporary_directory() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hermit-verification-report-consumers-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).expect("create temporary directory");
    path
}

fn measured_match() -> Value {
    // Exact evidence values from the measured ptrace report recorded in
    // ci/compat-envelope/cells.json for e69c0a62cecef9aa44e3810ae88c06ad24155048.
    serde_json::json!({
        "verified": true,
        "bitwise_parity": true,
        "verdict": "matched",
        "no_result_reason": null,
        "infrastructure_error": null,
        "comparison": {
            "strictness": "canonical",
            "display_name": "BitwiseInfoV1",
            "compare_logs": true,
            "compare_io_buffers": true,
            "log_scope": "info",
            "record_envelope": "all_records_v1",
            "virtualize_time": true,
            "strip_lines": false,
            "canonicalize_addresses": false,
            "full_trace": false,
            "exact_remainder": true,
            "stripped_prefixes": [],
            "canonicalizations": [],
            "ignore_lines": false,
            "skip_commit": false,
            "skip_detlog": false
        },
        "compared_log_messages": {"left": 266, "right": 266},
        "dbt_counted_branches": null,
        "runtime": null,
        "guest_exit_code": 0,
        "guest_signal": null,
        "first_divergent_scheduler_turn": null,
        "first_divergent_virtual_nanoseconds": null,
        "first_divergent_record": null,
        "first_divergent_syscall": null,
        "first_divergent_left_message": null,
        "first_divergent_right_message": null
    })
}

fn write_report(path: &Path, report: &Value) {
    fs::write(path, format!("{report}\n")).expect("write verification report");
}

fn verdict(requirement: &str, path: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_verification-report"))
        .arg(requirement)
        .arg(path)
        .output()
        .expect("run verification-report")
}

#[test]
fn every_named_consumer_delegates_to_the_shared_typed_reader() {
    let root = root();
    assert_eq!(CONSUMERS.len(), 11, "the published consumer list changed");
    for consumer in CONSUMERS {
        let source = fs::read_to_string(root.join(consumer.path))
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", consumer.path));
        assert!(
            source.contains("VERIFICATION_REPORT_BIN"),
            "{} does not name the shared typed reader",
            consumer.path
        );
        assert!(
            source.contains("--verify-json"),
            "{} does not request the producer-owned report",
            consumer.path
        );
        assert!(
            source.matches(consumer.invocation).count() >= consumer.minimum_invocations,
            "{} does not use the typed reader's {} result at every decision site",
            consumer.path,
            consumer.requirement
        );
        assert!(
            !source.lines().any(|line| {
                line.contains("Determinism verified")
                    && (line.contains("grep")
                        || line.contains(" in stderr")
                        || line.contains(" in result."))
            }),
            "{} still makes a functional decision from the banner",
            consumer.path
        );
    }
}

#[test]
fn a_real_match_and_a_typed_verdict_mutation_bracket_every_consumer() {
    let temporary = temporary_directory();
    let report_path = temporary.join("verify.json");
    let matched = measured_match();
    write_report(&report_path, &matched);

    for consumer in CONSUMERS {
        let accepted = verdict(consumer.requirement, &report_path);
        assert!(
            accepted.status.success(),
            "{} did not accept the measured typed match: {}",
            consumer.path,
            String::from_utf8_lossy(&accepted.stderr)
        );
    }

    let mut diverged = matched;
    diverged["verified"] = serde_json::json!(false);
    diverged["bitwise_parity"] = serde_json::json!(false);
    diverged["verdict"] = serde_json::json!("diverged");
    write_report(&report_path, &diverged);

    for consumer in CONSUMERS {
        let refused = verdict(consumer.requirement, &report_path);
        assert_eq!(
            refused.status.code(),
            Some(1),
            "{} ignored the mutated typed verdict: {}",
            consumer.path,
            String::from_utf8_lossy(&refused.stderr)
        );
    }

    let mut infrastructure_error = measured_match();
    infrastructure_error["verified"] = serde_json::json!(false);
    infrastructure_error["bitwise_parity"] = serde_json::json!(false);
    infrastructure_error["verdict"] = serde_json::json!("infrastructure_error");
    infrastructure_error["comparison"] = serde_json::Value::Null;
    infrastructure_error["compared_log_messages"] = serde_json::Value::Null;
    infrastructure_error["infrastructure_error"] =
        serde_json::json!({"kind": "skid_overshoot", "count": 2});
    write_report(&report_path, &infrastructure_error);

    for consumer in CONSUMERS {
        let refused = verdict(consumer.requirement, &report_path);
        assert_eq!(
            refused.status.code(),
            Some(1),
            "{} accepted a typed infrastructure error: {}",
            consumer.path,
            String::from_utf8_lossy(&refused.stderr)
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("infrastructure_error"),
            "{} refused a typed infrastructure error without naming it: {}",
            consumer.path,
            String::from_utf8_lossy(&refused.stderr)
        );
    }
    fs::remove_dir_all(temporary).expect("remove temporary directory");
}

#[test]
fn current_shape_and_canonical_evidence_fail_by_name() {
    let temporary = temporary_directory();
    let report_path = temporary.join("verify.json");

    let mut unknown = measured_match();
    unknown["verdict"] = serde_json::json!("future_verdict");
    write_report(&report_path, &unknown);
    let refused = verdict("matched", &report_path);
    assert_eq!(refused.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("unknown variant `future_verdict`"),
        "unknown verdict must fail by name: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let mut incomplete = measured_match();
    incomplete
        .as_object_mut()
        .expect("object")
        .remove("guest_signal");
    write_report(&report_path, &incomplete);
    let refused = verdict("matched", &report_path);
    assert_eq!(refused.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("missing current producer field `guest_signal`"),
        "missing current field must fail by name: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let mut stripped = measured_match();
    stripped["bitwise_parity"] = serde_json::json!(false);
    stripped["comparison"]["strictness"] = serde_json::json!("stripped");
    write_report(&report_path, &stripped);
    let refused = verdict("canonical-match", &report_path);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("strictness=stripped"),
        "weakened comparison must fail by name: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    fs::remove_dir_all(temporary).expect("remove temporary directory");
}
