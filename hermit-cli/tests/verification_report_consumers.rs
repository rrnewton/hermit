/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

#[path = "../../scripts/lib/verified_command.rs"]
mod verified_command;

use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
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
        path: "tests/e2e/lib/applications/common.sh",
        requirement: "canonical-match",
        invocation: "\"$VERIFICATION_REPORT_BIN\" canonical-match \"$verdict_file\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/reproducible-builds/run.sh",
        requirement: "canonical-match",
        invocation: "\"$VERIFICATION_REPORT_BIN\" canonical-match \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/backend-parity/e9patch_corpus.py",
        requirement: "canonical-match",
        invocation: "verification_matched(hermit,",
        minimum_invocations: 2,
    },
    Consumer {
        path: "tests/backend-parity/run_matrix.py",
        requirement: "canonical-match",
        invocation: "verify_tier_from_json(verdict_path, require_canonical=True)",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/e2e/lib/data-handling/common.bash",
        requirement: "canonical-match",
        invocation: "\"$VERIFICATION_REPORT_BIN\" canonical-match \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/e2e/lib/determinism-stress/common.sh",
        requirement: "canonical-match",
        invocation: "\"$VERIFICATION_REPORT_BIN\" canonical-match \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/e2e/lib/language-runtimes/run.sh",
        requirement: "canonical-match",
        invocation: "\"$VERIFICATION_REPORT_BIN\" canonical-match \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/e2e/lib/system-utils/_common.sh",
        requirement: "canonical-match",
        invocation: "\"$VERIFICATION_REPORT_BIN\" canonical-match \"$VERIFY_REPORT\"",
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
        requirement: "canonical-match",
        invocation: "\"$VERIFICATION_REPORT_BIN\" canonical-match \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/qemu-boot/strict_l2_userspace_test.sh",
        requirement: "canonical-match",
        invocation: "\"$VERIFICATION_REPORT_BIN\" canonical-match \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/standalone/strict_setitimer.sh",
        requirement: "canonical-match",
        invocation: "\"$VERIFICATION_REPORT_BIN\" canonical-match \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/standalone/strict_timer_create.sh",
        requirement: "canonical-match",
        invocation: "\"$VERIFICATION_REPORT_BIN\" canonical-match \"$verify_report\"",
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

const ARTIFACT_CONSUMER_CHILD: &str = "HERMIT_ARTIFACT_CONSUMER_CHILD";
const ARTIFACT_CONSUMER_ENTRY_MARKER: &str = "HERMIT_ARTIFACT_CONSUMER_ENTRY_MARKER";
const ARTIFACT_CONSUMER_MODE: &str = "artifact";
const STANDALONE_CARGO_MODE: &str = "standalone-cargo";

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("make {} executable: {error}", path.display()));
}

fn run_artifact_consumer(root: &Path, pointer: &Path, marker: &Path) -> Output {
    Command::new(root.join("ci/run-with-hermit-e2e-artifact.sh"))
        .env("HERMIT_E2E_ARTIFACT_POINTER", pointer)
        .env(ARTIFACT_CONSUMER_CHILD, ARTIFACT_CONSUMER_MODE)
        .env(ARTIFACT_CONSUMER_ENTRY_MARKER, marker)
        .arg(std::env::current_exe().expect("locate the running integration-test binary"))
        .args([
            "--exact",
            "immutable_artifact_controls_a_real_integration_consumer",
            "--nocapture",
        ])
        .output()
        .expect("run the integration-test consumer through the artifact wrapper")
}

fn copy_binary_only_bundle(source: &Path, destination: &Path) {
    fs::create_dir_all(destination)
        .unwrap_or_else(|error| panic!("create {}: {error}", destination.display()));
    for name in ["hermit", "hermit.sha256", "kind"] {
        fs::copy(source.join(name), destination.join(name)).unwrap_or_else(|error| {
            panic!("copy {} into {}: {error}", name, destination.display())
        });
    }
}

#[test]
fn immutable_artifact_controls_a_real_integration_consumer() {
    match std::env::var(ARTIFACT_CONSUMER_CHILD).as_deref() {
        Ok(ARTIFACT_CONSUMER_MODE) => {
            let marker = std::env::var_os(ARTIFACT_CONSUMER_ENTRY_MARKER)
                .map(PathBuf::from)
                .expect("child invocation lacks its entry marker");
            fs::write(&marker, b"entered\n")
                .unwrap_or_else(|error| panic!("write {}: {error}", marker.display()));
            let output = Command::new(hermit_test::hermit_binary())
                .output()
                .expect("execute the Hermit binary selected by the shared test resolver");
            assert!(
                output.status.success(),
                "selected Hermit failed: {output:?}"
            );
            assert_eq!(output.stdout, b"expected-identity\n");
            return;
        }
        Ok(STANDALONE_CARGO_MODE) => {
            assert!(
                std::env::var_os("HERMIT_BIN").is_none(),
                "standalone Cargo child unexpectedly inherited HERMIT_BIN"
            );
            assert_eq!(
                hermit_test::hermit_binary(),
                Path::new(env!("CARGO_BIN_EXE_hermit")),
                "standalone Cargo did not select its compile-time Hermit binary"
            );
            let output = Command::new(hermit_test::hermit_binary())
                .arg("--version")
                .output()
                .expect("execute standalone Cargo's compiled Hermit binary");
            assert!(
                output.status.success(),
                "standalone Cargo's compiled Hermit --version failed: {output:?}"
            );
            return;
        }
        Ok(mode) => panic!("unknown artifact consumer child mode {mode:?}"),
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("artifact consumer child mode is not valid UTF-8")
        }
        Err(std::env::VarError::NotPresent) => {}
    }

    let root = root();
    if std::env::var_os("HERMIT_BIN").is_none_or(|path| path.is_empty()) {
        let standalone = Command::new(
            std::env::current_exe().expect("locate the running standalone integration-test binary"),
        )
        .env_remove("HERMIT_BIN")
        .env(ARTIFACT_CONSUMER_CHILD, STANDALONE_CARGO_MODE)
        .args([
            "--exact",
            "immutable_artifact_controls_a_real_integration_consumer",
            "--nocapture",
        ])
        .output()
        .expect("self-spawn the standalone Cargo integration-test consumer");
        assert!(
            standalone.status.success(),
            "standalone Cargo integration consumer failed:\n{}",
            String::from_utf8_lossy(&standalone.stderr)
        );
    }

    let temporary = temporary_directory();
    let mutable = temporary.join("target/debug/hermit");
    let bundles = temporary.join("target/ci/hermit-e2e-artifacts");
    let pointer = temporary.join("target/ci/hermit-e2e-artifact.path");
    fs::create_dir_all(mutable.parent().expect("mutable binary has a parent"))
        .expect("create mutable target directory");
    write_executable(&mutable, "#!/bin/sh\nprintf 'expected-identity\\n'\n");

    let published = Command::new(root.join("ci/publish-hermit-e2e-artifact.sh"))
        .args([mutable.as_path(), bundles.as_path(), pointer.as_path()])
        .output()
        .expect("publish the fixture Hermit artifact");
    assert!(
        published.status.success(),
        "fixture artifact publication failed:\n{}",
        String::from_utf8_lossy(&published.stderr)
    );
    let bundle = PathBuf::from(
        fs::read_to_string(&pointer)
            .expect("read fixture artifact pointer")
            .trim(),
    );

    let replacement = mutable.with_extension("next");
    write_executable(&replacement, "#!/bin/sh\nprintf 'wrong-relinked\\n'\n");
    fs::rename(&replacement, &mutable).expect("atomically relink mutable Hermit source");
    let relink_marker = temporary.join("relink-consumer-entered");
    let relink = run_artifact_consumer(&root, &pointer, &relink_marker);
    assert!(
        relink.status.success(),
        "real integration consumer followed the relinked mutable path:\n{}",
        String::from_utf8_lossy(&relink.stderr)
    );
    assert!(
        relink_marker.is_file(),
        "real integration consumer was never entered for the valid immutable artifact"
    );

    for (state, expected_reason) in [
        (
            "absent",
            "published Hermit is missing, empty, or non-executable",
        ),
        (
            "nonexec",
            "published Hermit is missing, empty, or non-executable",
        ),
        ("wrong-hash", "published Hermit hash mismatch"),
    ] {
        let fake_bundle = temporary.join(state).join(
            bundle
                .file_name()
                .expect("published bundle has an identity"),
        );
        copy_binary_only_bundle(&bundle, &fake_bundle);
        match state {
            "absent" => fs::remove_file(fake_bundle.join("hermit"))
                .expect("remove the fake artifact binary"),
            "nonexec" => fs::set_permissions(
                fake_bundle.join("hermit"),
                fs::Permissions::from_mode(0o644),
            )
            .expect("make the fake artifact binary non-executable"),
            "wrong-hash" => OpenOptions::new()
                .append(true)
                .open(fake_bundle.join("hermit"))
                .and_then(|mut file| file.write_all(b"corruption"))
                .expect("corrupt the fake artifact binary"),
            _ => unreachable!(),
        }
        let fake_pointer = temporary.join(format!("{state}.path"));
        fs::write(&fake_pointer, format!("{}\n", fake_bundle.display()))
            .expect("write fake artifact pointer");
        let marker = temporary.join(format!("{state}-consumer-entered"));
        let refused = run_artifact_consumer(&root, &fake_pointer, &marker);
        assert_eq!(
            refused.status.code(),
            Some(2),
            "{state} artifact returned the wrong status:\n{}",
            String::from_utf8_lossy(&refused.stderr)
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains(expected_reason),
            "{state} artifact refusal did not name {expected_reason:?}:\n{}",
            String::from_utf8_lossy(&refused.stderr)
        );
        assert!(
            !marker.exists(),
            "{state} artifact entered the protected integration consumer"
        );
    }

    fs::remove_dir_all(temporary).expect("remove artifact-contract temporary directory");
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

fn current_synthetic_match() -> Value {
    // Synthetic current report: the historical measured fixture above remains
    // unchanged. Both synthetic executions exit zero with empty byte streams.
    let mut report = measured_match();
    let output = serde_json::json!({"exit_code": 0, "signal": null,
        "stdout_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "stdout_bytes": 0,
        "stderr_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "stderr_bytes": 0});
    report["compared_outputs"] = serde_json::json!({"left": output, "right": output});
    report["comparison"]["canonicalize_addresses"] = serde_json::json!(true);
    report["comparison"]["full_trace"] = serde_json::json!(true);
    report["comparison"]["stripped_prefixes"] = serde_json::json!(["real-wall-clock-prefix/v1"]);
    report["comparison"]["canonicalizations"] =
        serde_json::json!(["host-address-to-first-appearance-ordinal/v1"]);
    report["compared_log_messages"] = serde_json::json!({"left": 2, "right": 2});
    report
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
    assert_eq!(CONSUMERS.len(), 13, "the published consumer list changed");
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
        if consumer.path == "tests/backend-parity/run_matrix.py" {
            assert_eq!(consumer.requirement, "canonical-match");
            assert!(source.contains(
                "requirement = \"canonical-match\" if require_canonical else \"matched\""
            ));
            assert!(
                source
                    .contains("[str(VERIFICATION_REPORT_BIN), \"--json\", requirement, str(path)]")
            );
        }
        if consumer.path == "tests/backend-parity/e9patch_corpus.py" {
            assert_eq!(consumer.requirement, "canonical-match");
            assert!(source.contains(
                "[str(verification_report_bin(hermit)), \"canonical-match\", str(report)]"
            ));
        }
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
fn current_match_and_a_typed_verdict_mutation_bracket_every_consumer() {
    let temporary = temporary_directory();
    let report_path = temporary.join("verify.json");
    let historical = measured_match();
    assert!(historical.get("compared_outputs").is_none());
    let retained = hermit::canonical_verdict::VerificationReport::from_json_slice(
        &serde_json::to_vec(&historical).unwrap(),
    )
    .unwrap();
    assert!(retained.compared_outputs.is_none());
    write_report(&report_path, &historical);
    for consumer in CONSUMERS {
        let refused = verdict(consumer.requirement, &report_path);
        assert_eq!(refused.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("compared_outputs"));
    }
    let matched = current_synthetic_match();
    write_report(&report_path, &matched);

    for consumer in CONSUMERS {
        let accepted = verdict(consumer.requirement, &report_path);
        assert!(
            accepted.status.success(),
            "{} did not accept the current synthetic typed match: {}",
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
            Some(1), // EXIT-CLASS: verification-report requirement unmet.
            "{} ignored the mutated typed verdict: {}",
            consumer.path,
            String::from_utf8_lossy(&refused.stderr)
        );
    }

    let mut infrastructure_error = current_synthetic_match();
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
            Some(1), // EXIT-CLASS: verification-report requirement unmet.
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

    let mut unknown = current_synthetic_match();
    unknown["verdict"] = serde_json::json!("future_verdict");
    write_report(&report_path, &unknown);
    let refused = verdict("matched", &report_path);
    assert_eq!(refused.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("unknown variant `future_verdict`"),
        "unknown verdict must fail by name: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let mut incomplete = current_synthetic_match();
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

    let mut stripped = current_synthetic_match();
    stripped["bitwise_parity"] = serde_json::json!(false);
    stripped["comparison"]["strictness"] = serde_json::json!("stripped");
    write_report(&report_path, &stripped);
    let refused = verdict("canonical-match", &report_path);
    // EXIT-CLASS: verification-report canonical-match requirement unmet.
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("strictness=stripped"),
        "weakened comparison must fail by name: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    fs::remove_dir_all(temporary).expect("remove temporary directory");
}

#[test]
fn qemu_boot_consumer_rejects_noncanonical_and_unequal_match_claims() {
    let consumer = CONSUMERS
        .iter()
        .find(|consumer| consumer.path == "tests/qemu-boot/strict_l2_test.sh")
        .unwrap();
    assert_eq!(consumer.requirement, "canonical-match");
    let script = fs::read_to_string(root().join(consumer.path)).unwrap();
    assert_eq!(script.matches(consumer.invocation).count(), 1);
    let temporary = temporary_directory();
    let path = temporary.join("verify.json");
    for case in ["matched", "stripped", "no-log-comparison", "unequal-counts"] {
        let mut report = current_synthetic_match();
        report["compared_log_messages"] = serde_json::json!({"left": 123, "right": 123});
        match case {
            "matched" => {}
            "stripped" => report["comparison"]["strictness"] = serde_json::json!("stripped"),
            "no-log-comparison" => report["comparison"]["compare_logs"] = serde_json::json!(false),
            "unequal-counts" => report["compared_log_messages"]["right"] = serde_json::json!(124),
            _ => unreachable!(),
        }
        write_report(&path, &report);
        let output = verdict(consumer.requirement, &path);
        assert_eq!(
            output.status.code(),
            Some(if case == "matched" { 0 } else { 1 }),
            "{case}: {output:?}"
        );
        if case == "unequal-counts" {
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("canonical match has unequal compared log-message counts: 123/124")
            );
        }
    }
    fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn remaining_shell_consumers_require_current_canonical_match_evidence() {
    let selected_paths = [
        "tests/e2e/lib/data-handling/common.bash",
        "tests/e2e/lib/determinism-stress/common.sh",
        "tests/e2e/lib/language-runtimes/run.sh",
        "tests/e2e/lib/system-utils/_common.sh",
        "tests/qemu-boot/strict_l2_userspace_test.sh",
        "tests/standalone/strict_setitimer.sh",
        "tests/standalone/strict_timer_create.sh",
    ];
    let temporary = temporary_directory();
    let path = temporary.join("verify.json");
    for selected_path in selected_paths {
        let consumer = CONSUMERS
            .iter()
            .find(|consumer| consumer.path == selected_path)
            .expect("retain every named shell consumer");
        assert_eq!(consumer.requirement, "canonical-match", "{selected_path}");
        let script = fs::read_to_string(root().join(selected_path)).unwrap();
        assert_eq!(
            script.matches(consumer.invocation).count(),
            1,
            "{selected_path}"
        );
        assert!(
            !script.contains("\"$VERIFICATION_REPORT_BIN\" matched "),
            "{selected_path} still admits the weaker requirement"
        );
        for case in [
            "matched",
            "stripped",
            "no-log-comparison",
            "zero-counts",
            "unequal-counts",
            "missing-report",
            "invalid-json",
            "missing-current-outputs",
        ] {
            let mut report = current_synthetic_match();
            report["compared_log_messages"] = serde_json::json!({"left": 123, "right": 123});
            match case {
                "stripped" => {
                    report["comparison"]["strictness"] = serde_json::json!("stripped");
                }
                "no-log-comparison" => {
                    report["comparison"]["compare_logs"] = serde_json::json!(false);
                }
                "zero-counts" => {
                    report["compared_log_messages"] = serde_json::json!({"left": 0, "right": 0});
                }
                "unequal-counts" => {
                    report["compared_log_messages"]["right"] = serde_json::json!(124);
                }
                "missing-current-outputs" => {
                    report.as_object_mut().unwrap().remove("compared_outputs");
                }
                "matched" | "missing-report" | "invalid-json" => {}
                _ => unreachable!(),
            }
            write_report(&path, &report);
            let expected_status = match case {
                "matched" => 0,
                "missing-report" => {
                    fs::remove_file(&path).unwrap();
                    2
                }
                "invalid-json" => {
                    fs::write(&path, b"{").unwrap();
                    2
                }
                "missing-current-outputs" => 2,
                _ => 1,
            };
            let output = verdict(consumer.requirement, &path);
            assert_eq!(
                output.status.code(),
                Some(expected_status),
                "{selected_path} {case}: {output:?}"
            );
            if case == "unequal-counts" {
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains(
                        "canonical match has unequal compared log-message counts: 123/124"
                    ),
                    "{selected_path}: {output:?}"
                );
            }
        }
    }
    fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn json_output_retains_failure_evidence_without_satisfying_a_match_requirement() {
    let temporary = temporary_directory();
    let report_path = temporary.join("verify.json");
    for name in ["matched", "diverged", "no_result", "infrastructure_error"] {
        let mut report = current_synthetic_match();
        report["verdict"] = serde_json::json!(name);
        if name != "matched" {
            report["verified"] = serde_json::json!(false);
            report["bitwise_parity"] = serde_json::json!(false);
        }
        if matches!(name, "no_result" | "infrastructure_error") {
            report["comparison"] = Value::Null;
            report["compared_log_messages"] = Value::Null;
            report["compared_outputs"] = Value::Null;
        }
        if name == "infrastructure_error" {
            report["infrastructure_error"] =
                serde_json::json!({"kind": "skid_overshoot", "count": 2});
        }
        write_report(&report_path, &report);
        for requirement in ["matched", "canonical-match"] {
            let output = Command::new(env!("CARGO_BIN_EXE_verification-report"))
                .args(["--json", requirement])
                .arg(&report_path)
                .output()
                .expect("read report with JSON output");
            assert_eq!(
                output.status.code(),
                Some(if name == "matched" { 0 } else { 1 }),
                "{name} must retain its {requirement} exit status: {output:?}"
            );
            let parsed: Value = serde_json::from_slice(&output.stdout).expect("typed JSON");
            for field in [
                "verified",
                "bitwise_parity",
                "verdict",
                "infrastructure_error",
                "comparison",
                "compared_log_messages",
                "guest_exit_code",
                "guest_signal",
            ] {
                assert_eq!(parsed[field], report[field], "{name}: {field}");
            }
            if name == "matched" {
                assert!(output.stderr.is_empty(), "{output:?}");
            } else {
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains(name),
                    "failure must still be named: {output:?}"
                );
            }
        }
    }

    for malformed in ["missing field", "unknown verdict", "invalid cause"] {
        let mut report = current_synthetic_match();
        match malformed {
            "missing field" => {
                report.as_object_mut().unwrap().remove("guest_signal");
            }
            "unknown verdict" => report["verdict"] = serde_json::json!("future_verdict"),
            "invalid cause" => {
                report["verified"] = serde_json::json!(false);
                report["bitwise_parity"] = serde_json::json!(false);
                report["verdict"] = serde_json::json!("infrastructure_error");
                report["infrastructure_error"] =
                    serde_json::json!({"kind": "skid_overshoot", "count": 0});
            }
            _ => unreachable!(),
        }
        write_report(&report_path, &report);
        let output = Command::new(env!("CARGO_BIN_EXE_verification-report"))
            .args(["--json", "matched"])
            .arg(&report_path)
            .output()
            .expect("refuse malformed report");
        assert_eq!(output.status.code(), Some(2), "{malformed}: {output:?}");
        assert!(output.stdout.is_empty(), "{malformed}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("REFUSED"),
            "{malformed}: {output:?}"
        );
    }
    fs::remove_dir_all(temporary).expect("remove temporary directory");
}

#[test]
fn generated_verification_commands_validate_fresh_evidence_before_guest_status() {
    let temporary = temporary_directory();
    let cell = temporary.join("cell with spaces");
    fs::create_dir_all(cell.join("captures")).unwrap();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_verification-report"),
        temporary.join("verification-report"),
    )
    .unwrap();
    let producer = temporary.join("producer");
    write_executable(
        &producer,
        r#"#!/usr/bin/env bash
set -eu
printf 'entered\n' >"$PRODUCER_ENTRY"
case "$REPORT_MODE" in
  copy) cp -- "$REPORT_INPUT" "$1";;
  empty) : >"$1";;
  absent) :;;
  *) exit 99;;
esac
exit "$PRODUCER_STATUS"
"#,
    );
    let input = temporary.join("input.json");
    let entry = temporary.join("producer-entered");
    let valid = current_synthetic_match();
    let mut status23 = valid.clone();
    status23["guest_exit_code"] = serde_json::json!(23);
    status23["compared_outputs"]["left"]["exit_code"] = serde_json::json!(23);
    status23["compared_outputs"]["right"]["exit_code"] = serde_json::json!(23);
    let mut cases = vec![
        ("valid", valid.clone(), "copy", 0, Some(0)),
        ("guest23", status23, "copy", 23, Some(23)),
        ("absent", valid.clone(), "absent", 23, None),
        ("empty", valid.clone(), "empty", 23, None),
        (
            "invalid-json-type",
            Value::String("not a report".into()),
            "copy",
            23,
            None,
        ),
    ];
    for field in [
        "strictness",
        "compare_logs",
        "verified",
        "missing",
        "zero",
        "unequal",
    ] {
        let mut report = valid.clone();
        match field {
            "strictness" => report["comparison"]["strictness"] = serde_json::json!("stripped"),
            "compare_logs" => report["comparison"]["compare_logs"] = serde_json::json!(false),
            "verified" => report["verified"] = serde_json::json!(false),
            "missing" => {
                report.as_object_mut().unwrap().remove("guest_signal");
            }
            "zero" => report["compared_log_messages"] = serde_json::json!({"left": 0, "right": 0}),
            "unequal" => report["compared_log_messages"]["right"] = serde_json::json!(3),
            _ => unreachable!(),
        }
        cases.push((field, report, "copy", 23, None));
    }
    for mode in ["verify", "replay", "chaos"] {
        let report_path = cell.join("captures").join(if mode == "chaos" {
            "verify-seed-17.json"
        } else {
            "verify.json"
        });
        let command = verified_command::hermit_verification_command(
            mode,
            Some(17),
            "\"$FAKE_PRODUCER\" \"$verify_report\"",
        );
        for (name, report, report_mode, status, expected) in &cases {
            write_report(&input, report);
            // A report from a previous attempt must never qualify this one.
            write_report(&report_path, &valid);
            let output = Command::new("bash")
                .args(["-c", &command])
                .env("cell", &cell)
                .env("hermit_bin", temporary.join("synthetic-hermit"))
                .env_remove("VERIFICATION_REPORT_BIN")
                .env("FAKE_PRODUCER", &producer)
                .env("REPORT_INPUT", &input)
                .env("REPORT_MODE", report_mode)
                .env("PRODUCER_STATUS", status.to_string())
                .env("PRODUCER_ENTRY", &entry)
                .output()
                .expect("execute the actual generated shell wrapper and typed reader");
            assert!(entry.is_file(), "{mode}/{name} did not reach its producer");
            fs::remove_file(&entry).unwrap();
            if let Some(code) = expected {
                assert_eq!(
                    output.status.code(),
                    Some(*code),
                    "{mode}/{name}: {output:?}"
                );
            } else {
                assert!(
                    matches!(output.status.code(), Some(1 | 2)),
                    "{mode}/{name}: {output:?}"
                );
                assert!(String::from_utf8_lossy(&output.stderr).contains("verification-report"));
            }
        }
        let output = Command::new("bash")
            .args(["-c", &command])
            .env("cell", &cell)
            .env("hermit_bin", temporary.join("synthetic-hermit"))
            .env("VERIFICATION_REPORT_BIN", temporary.join("absent-reader"))
            .env("FAKE_PRODUCER", &producer)
            .env("PRODUCER_ENTRY", &entry)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(
            !entry.exists(),
            "missing reader must refuse before the producer"
        );
    }
    assert_eq!(
        verified_command::hermit_verification_command("custom", None, "exit 23"),
        "exit 23"
    );
    let generator = fs::read_to_string(root().join("scripts/manifest-to-commands.rs")).unwrap();
    assert!(
        generator.contains("verified_command::hermit_verification_command(mode, seed, &command)")
    );
    fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn application_shell_consumer_uses_the_real_typed_reader() {
    let output = Command::new("bash")
        .arg(root().join("tests/e2e/lib/applications/test_verdict_discrimination.sh"))
        .env(
            "VERIFICATION_REPORT_BIN",
            env!("CARGO_BIN_EXE_verification-report"),
        )
        .env_remove("HERMIT_E2E_EMPTY_WORKDIR")
        .output()
        .expect("run synthetic producers through the actual application shell consumer");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    for case in [
        "strict-parity",
        "valid-guest-status23",
        "contradictory-compare_logs",
        "contradictory-strictness",
        "contradictory-verified",
        "empty-report",
        "infrastructure-error",
        "pinned-root-workdir",
    ] {
        assert!(
            stdout.contains(case),
            "missing actual case {case}: {stdout}"
        );
    }
    assert!(stdout.contains("all verdict-discrimination cases passed"));
}

#[test]
fn canonical_reader_rejects_unequal_counts_while_matched_and_inspection_stay_distinct() {
    let temporary = temporary_directory();
    let path = temporary.join("verify.json");
    let mut report = current_synthetic_match();
    report["compared_log_messages"]["right"] = serde_json::json!(3);
    write_report(&path, &report);
    // `matched` is a weaker named requirement; it is not canonical qualification.
    let matched = verdict("matched", &path);
    assert_eq!(matched.status.code(), Some(0), "{matched:?}");
    let canonical = Command::new(env!("CARGO_BIN_EXE_verification-report"))
        .args(["--json", "canonical-match"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(canonical.status.code(), Some(1), "{canonical:?}");
    assert!(
        String::from_utf8_lossy(&canonical.stderr)
            .contains("canonical match has unequal compared log-message counts: 2/3")
    );
    let inspected: Value = serde_json::from_slice(&canonical.stdout).unwrap();
    assert_eq!(
        inspected["compared_log_messages"],
        report["compared_log_messages"]
    );
    report["compared_log_messages"]["right"] = serde_json::json!(2);
    write_report(&path, &report);
    let canonical = verdict("canonical-match", &path);
    assert_eq!(canonical.status.code(), Some(0), "{canonical:?}");
    fs::remove_dir_all(temporary).unwrap();
}

#[test]
fn reproducible_build_consumer_keeps_artifact_and_typed_verdict_requirements() {
    let temporary = temporary_directory();
    let script = temporary.join("tests/reproducible-builds/run.sh");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::copy(root().join("tests/reproducible-builds/run.sh"), &script).unwrap();
    let fixture = script.parent().unwrap().join("build-time-0.1.3");
    fs::create_dir_all(fixture.join("target/release/deps")).unwrap();
    fs::write(
        fixture.join("target/release/deps/libbuild_time-control.so"),
        b"synthetic",
    )
    .unwrap();
    let tools = temporary.join("tools");
    fs::create_dir(&tools).unwrap();
    // Controlled command outputs exercise the real shell pipeline. No compiler,
    // Hermit guest, proc macro, or VM executes in this native consumer control.
    write_executable(&tools.join("cargo"), "#!/usr/bin/env bash\nexit 0\n");
    write_executable(&tools.join("sleep"), "#!/usr/bin/env bash\nexit 0\n");
    write_executable(
        &tools.join("rustc"),
        r#"#!/usr/bin/env python3
import os,pathlib,sys
if sys.argv[1:] == ['--version']:
    print('rustc nightly synthetic-control')
else:
    output=pathlib.Path(sys.argv[sys.argv.index('-o')+1])
    output.write_text('same' if os.environ['NATIVE_EQUAL']=='1' else output.name)
"#,
    );
    let producer = tools.join("hermit");
    write_executable(
        &producer,
        r#"#!/usr/bin/env python3
import os,pathlib,shutil,sys
args=sys.argv[1:]
if '--verify-json' in args:
    pathlib.Path(os.environ['PRODUCER_ENTRY']).write_text('entered')
    report=pathlib.Path(args[args.index('--verify-json')+1])
    mode=os.environ['REPORT_MODE']
    if mode=='copy': shutil.copyfile(os.environ['REPORT_INPUT'],report)
    elif mode=='empty': report.write_bytes(b'')
    elif mode!='absent': raise AssertionError(mode)
    sys.exit(int(os.environ['PRODUCER_STATUS']))
output=pathlib.Path(args[args.index('-o')+1])
output.write_text(output.name if os.environ['HERMIT_DIFFERENT']=='1' else 'same')
"#,
    );
    let valid = current_synthetic_match();
    let input = temporary.join("input.json");
    let entry = temporary.join("verification-entered");
    let artifacts = fixture.join("target/reproducible-builds");
    fs::create_dir_all(&artifacts).unwrap();
    let mut contradictory = valid.clone();
    contradictory["comparison"]["compare_logs"] = serde_json::json!(false);
    let mut status23 = valid.clone();
    status23["guest_exit_code"] = serde_json::json!(23);
    status23["compared_outputs"]["left"]["exit_code"] = serde_json::json!(23);
    status23["compared_outputs"]["right"]["exit_code"] = serde_json::json!(23);
    for (
        name,
        report,
        report_mode,
        guest_status,
        native_equal,
        hermit_different,
        expected,
        entered,
    ) in [
        ("valid", valid.clone(), "copy", 0, "0", "0", 0, true),
        ("guest23", status23, "copy", 23, "0", "0", 23, true),
        (
            "contradictory",
            contradictory,
            "copy",
            23,
            "0",
            "0",
            1,
            true,
        ),
        ("absent", valid.clone(), "absent", 23, "0", "0", 1, true),
        ("empty", valid.clone(), "empty", 23, "0", "0", 1, true),
        ("native-equal", valid.clone(), "copy", 0, "1", "0", 1, false),
        (
            "hermit-different",
            valid.clone(),
            "copy",
            0,
            "0",
            "1",
            1,
            false,
        ),
    ] {
        write_report(&input, &report);
        write_report(&artifacts.join("verify.json"), &valid);
        let output = Command::new("bash")
            .arg(&script)
            .env(
                "PATH",
                format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
            )
            .env("HERMIT_BIN", &producer)
            .env(
                "VERIFICATION_REPORT_BIN",
                env!("CARGO_BIN_EXE_verification-report"),
            )
            .env("REPORT_INPUT", &input)
            .env("REPORT_MODE", report_mode)
            .env("PRODUCER_STATUS", guest_status.to_string())
            .env("PRODUCER_ENTRY", &entry)
            .env("NATIVE_EQUAL", native_equal)
            .env("HERMIT_DIFFERENT", hermit_different)
            .output()
            .expect("execute actual artifact checks and typed-report consumption");
        assert_eq!(output.status.code(), Some(expected), "{name}: {output:?}");
        assert_eq!(entry.exists(), entered, "{name}: {output:?}");
        if entered {
            fs::remove_file(&entry).unwrap();
        }
        assert_eq!(
            String::from_utf8_lossy(&output.stdout)
                .contains("PASS: native artifacts differ; strict ptrace Hermit artifacts match."),
            expected == 0,
            "{name}: {output:?}"
        );
        if name == "native-equal" {
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("Native builds unexpectedly matched")
            );
        }
        if name == "hermit-different" {
            assert!(String::from_utf8_lossy(&output.stderr).contains("Hermit builds differed"));
        }
    }
    fs::remove_dir_all(temporary).unwrap();
}
