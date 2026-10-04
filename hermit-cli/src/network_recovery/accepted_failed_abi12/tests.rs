use std::process::Command;

use super::*;

struct Fixture {
    _temp: tempfile::TempDir,
    directory: PathBuf,
    metadata: Value,
    prefix: Vec<u8>,
    prefix_values: Vec<Value>,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("fixture");
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/test_accepted_failed_abi12_resource_recovery.py");
        let output = Command::new("python3")
            .arg("-B")
            .arg(script)
            .arg("--fixture")
            .arg(&directory)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "actual controlled producer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: Value =
            serde_json::from_slice(&std::fs::read(directory.join("metadata.json")).unwrap())
                .unwrap();
        let terminal = directory.join("accepted").join(format!(
            "accepted-b1-{}.terminal.jsonl",
            metadata["accepted_label"].as_str().unwrap()
        ));
        let bytes = std::fs::read(terminal).unwrap();
        let rows = raw_rows(&bytes, 5).unwrap();
        let prefix = bytes[..rows[0].len() + rows[1].len() + rows[2].len()].to_vec();
        let prefix_values = rows[..3]
            .iter()
            .map(|row| serde_json::from_slice(row).unwrap())
            .collect();
        Self {
            _temp: temp,
            directory,
            metadata,
            prefix,
            prefix_values,
        }
    }
    fn context(&self) -> ResourceRecoveryContext {
        ResourceRecoveryContext::open(
            Path::new(self.metadata["accepted_root"].as_str().unwrap()),
            Path::new(self.metadata["unix_root"].as_str().unwrap()),
            Path::new(self.metadata["pin_root"].as_str().unwrap()),
        )
        .unwrap()
    }
    fn label(&self) -> &str {
        self.metadata["accepted_label"].as_str().unwrap()
    }
    fn terminal(&self) -> PathBuf {
        self.directory
            .join("accepted")
            .join(format!("accepted-b1-{}.terminal.jsonl", self.label()))
    }
    fn rows(&self) -> Vec<Value> {
        raw_rows(&std::fs::read(self.terminal()).unwrap(), 5)
            .unwrap()
            .into_iter()
            .map(|row| serde_json::from_slice(row).unwrap())
            .collect()
    }
    fn rewrite(&self, rows: &[Value]) {
        let original_prefix = rows.len() >= 3 && rows[..3] == self.prefix_values[..];
        let mut bytes = if original_prefix {
            self.prefix.clone()
        } else {
            Vec::new()
        };
        for value in rows.iter().skip(if original_prefix { 3 } else { 0 }) {
            bytes.extend(serde_json::to_vec(value).unwrap());
            bytes.push(b'\n');
        }
        std::fs::write(self.terminal(), bytes).unwrap();
    }
    fn bind_rewritten_prefix(rows: &mut [Value]) {
        let mut prefix = Vec::new();
        for row in &rows[..3] {
            prefix.extend(serde_json::to_vec(row).unwrap());
            prefix.push(b'\n');
        }
        rows[3][7][4][0][6] = serde_json::json!(prefix.len());
        rows[3][7][4][0][9] = digest(&prefix).into();
        let mut intent = serde_json::to_vec(&rows[3]).unwrap();
        intent.push(b'\n');
        rows[4][1] = digest(&intent).into();
    }
}

#[test]
fn actual_writer_certificate_is_resource_only_and_preserves_old_resolvers() {
    let fixture = Fixture::new();
    let context = fixture.context();
    resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap();
    assert!(context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));
    assert!(
        context
            .resolve(0, context.accepted.file.as_fd(), fixture.label())
            .is_err()
    );
    assert!(
        super::super::accepted_only::resolve(
            &context,
            context.accepted.file.as_fd(),
            fixture.label()
        )
        .is_err()
    );
    assert!(
        super::super::accepted_failed::resolve(
            &context,
            context.accepted.file.as_fd(),
            fixture.label()
        )
        .is_err()
    );
    assert!(
        super::super::accepted_abi11::resolve(
            &context,
            context.accepted.file.as_fd(),
            fixture.label()
        )
        .is_err()
    );
    assert!(!context.unix_resolved(
        context.unix.file.as_fd(),
        context.pins.file.as_fd(),
        fixture.label()
    ));
    let root =
        crate::unix_guard_package::RecoveryDeploymentRoot::open_at(&context.accepted.path).unwrap();
    let artifact: detcore::network_runtime::ProviderArtifact =
        serde_json::from_value(fixture.rows()[0]["artifact"].clone()).unwrap();
    assert!(
        crate::accepted_terminal::validate_accepted_receipt(&root, fixture.label(), &artifact)
            .is_err()
    );
    assert_eq!(fixture.metadata["result"]["execution_success"], false);
    assert_eq!(fixture.metadata["result"]["normal_terminal_success"], false);
    assert_eq!(digest(super::super::PRODUCER), DEPENDENCY);
}

#[test]
fn malformed_source_bound_certificate_never_discharges_admission() {
    let fixture = Fixture::new();
    let original = fixture.rows();
    let context = fixture.context();
    let mutations: Vec<fn(&mut Vec<Value>)> = vec![
        |r| r[3][0] = "unknown-intent".into(),
        |r| r[4][0] = "hermit-failed-resource-result-v1".into(),
        |r| r[3][1] = "00000000-0000-0000-0000-000000000000".into(),
        |r| r[3][4] = serde_json::json!(u32::MAX),
        |r| r[3][5] = "0".repeat(64).into(),
        |r| r[3][6] = "0".repeat(64).into(),
        |r| r[3][7][0] = "/wrong-root".into(),
        |r| r[3][7][1][1] = 0.into(),
        |r| r[3][7][2] = "f".repeat(32).into(),
        |r| r[3][7][3] = "f".repeat(32).into(),
        |r| r[3][7][4][1][9] = "0".repeat(64).into(),
        |r| r[3][7][5][6] = 23.into(),
        |r| r[3][7][6] = 0.into(),
        |r| r[3][8][0][1][0] = "f".repeat(32).into(),
        |r| r[3][8][0][2][0][1] = "active".into(),
        |r| r[3][8][0][3] = 1.into(),
        |r| r[3][8][0][6] = libc::EACCES.into(),
        |r| {
            r[3][9].as_array_mut().unwrap().pop();
        },
        |r| r[3][9][1] = r[3][9][0].clone(),
        |r| r[3][9][0][0] = 1.into(),
        |r| r[3][10] = "0".repeat(64).into(),
        |r| r[3][11] = "normal-success".into(),
        |r| r[4][1] = "0".repeat(64).into(),
        |r| r[4][3] = 0.into(),
        |r| r[4][4] = u64::MAX.into(),
        |r| r[4][5][0] = 0.into(),
        |r| r[4][5][1] = 0.into(),
        |r| r[4][5][2] = 1.into(),
        |r| r[4][5][3] = 1.into(),
        |r| r[4][5][4] = "0".repeat(64).into(),
        |r| r[4][5][5] = "0".repeat(64).into(),
        |r| r[4][6] = 125.into(),
        |r| {
            r[4][7].as_array_mut().unwrap().pop();
        },
        |r| {
            r[4][7][0][2].as_array_mut().unwrap().pop();
        },
        |r| r[4][7][0][2][1] = r[4][7][0][2][0].clone(),
        |r| r[4][7][1][2][0][3] = libc::EPERM.into(),
        |r| r[4][7][1][2][0][3] = 0.into(),
        |r| r[4][7][1][0] = 0.into(),
        |r| r[4][8] = u64::MAX.into(),
        |r| r[4][9][1][1][3] = 0.into(),
        |r| r[4][9][0][4] = 0.into(),
        |r| r[4][10] = "0".repeat(64).into(),
        |r| r[4][11] = "0".repeat(64).into(),
    ];
    let predicates = [
        "protocol",
        "protocol",
        "boot or owner",
        "boot or owner",
        "producer or raw intent",
        "producer or raw intent",
        "configured source",
        "configured source",
        "configured source",
        "configured source",
        "original file length or digest",
        "source artifact or close",
        "source artifact or close",
        "original actor or observation",
        "manager is not",
        "original actor or observation",
        "original actor or observation",
        "original inventory or profile",
        "original inventory or profile",
        "original inventory or profile",
        "original inventory or profile",
        "original inventory or profile",
        "producer or raw intent",
        "fresh verification interval",
        "fresh verification interval",
        "actual scanner or wait",
        "actual scanner or wait",
        "actual scanner or wait",
        "actual scanner or wait",
        "actual scanner or wait",
        "actual scanner or wait",
        "actual scanner or wait",
        "array arity",
        "array arity",
        "scan lacks an original absent ID",
        "scan lacks an original absent ID",
        "scan lacks an original absent ID",
        "scan chronology",
        "fresh verification interval",
        "original actor or observation",
        "original actor or observation",
        "producer or raw intent",
        "producer or raw intent",
    ];
    assert_eq!(mutations.len(), predicates.len());
    for (index, (change, predicate)) in mutations.into_iter().zip(predicates).enumerate() {
        let mut rows = original.clone();
        change(&mut rows);
        // Keep the raw intent hash correct so semantic errors are independently exercised.
        if index != 22 {
            let mut intent = serde_json::to_vec(&rows[3]).unwrap();
            intent.push(b'\n');
            rows[4][1] = digest(&intent).into();
        }
        fixture.rewrite(&rows);
        let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
        assert!(
            error.to_string().contains(predicate),
            "mutation {index}: expected {predicate}, got {error}"
        );
        assert!(
            !context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()),
            "admission mutation {index}"
        );
    }
    fixture.rewrite(&original);
    resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap();
}

#[test]
fn joint_zero_artifact_prefix_and_certificate_cannot_pass_original_parser() {
    let fixture = Fixture::new();
    let original = fixture.rows();
    let context = fixture.context();
    for key in [
        "contract_sha256",
        "object_sha256",
        "library_sha256",
        "btf_sha256",
    ] {
        let mut rows = original.clone();
        for index in 0..2 {
            let artifact = if index == 0 {
                &mut rows[0]["artifact"]
            } else {
                &mut rows[1]["observed"]["artifact"]
            };
            if key == "contract_sha256" {
                artifact["topology"][key] = serde_json::json!(vec![0; 32]);
            } else {
                artifact[key] = serde_json::json!(vec![0; 32]);
            }
        }
        Fixture::bind_rewritten_prefix(&mut rows);
        fixture.rewrite(&rows);
        let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
        assert!(
            error.to_string().contains("identity") || error.to_string().contains("digest"),
            "{error}"
        );
    }
}

#[test]
fn partial_and_extra_rows_and_foreign_actual_root_refuse() {
    let fixture = Fixture::new();
    let original = fixture.rows();
    let context = fixture.context();
    for count in 3..=4 {
        fixture.rewrite(&original[..count]);
        assert!(!context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));
    }
    let mut extra = original.clone();
    extra.push(original[4].clone());
    fixture.rewrite(&extra);
    assert!(!context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));
    fixture.rewrite(&original);
    assert!(!context.accepted_resolved(context.unix.file.as_fd(), fixture.label()));
    resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap();
}

#[test]
fn changed_prefix_digest_without_rebinding_remains_refused() {
    let fixture = Fixture::new();
    let context = fixture.context();
    let mut rows = fixture.rows();
    // A valid JSON spelling change preserves meaning but must still match the
    // original exact-prefix hash. No digest repair is performed here.
    let original = std::fs::read(fixture.terminal()).unwrap();
    let split = original.iter().position(|byte| *byte == b'\n').unwrap();
    let mut changed = original[..split].to_vec();
    changed.push(b' ');
    changed.extend(&original[split..]);
    std::fs::write(fixture.terminal(), changed).unwrap();
    let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
    assert!(
        error.to_string().contains("original file length or digest"),
        "{error}"
    );
    fixture.rewrite(&rows);
    resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap();
    rows[4][1] = "0".repeat(64).into();
    fixture.rewrite(&rows);
    let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
    assert!(
        error.to_string().contains("producer or raw intent"),
        "{error}"
    );
}

#[test]
fn joint_original_and_certificate_cgroup_change_refuses_exact_profile() {
    let fixture = Fixture::new();
    let context = fixture.context();
    let mut rows = fixture.rows();
    let foreign = format!(
        "/sys/fs/cgroup/foreign/{}",
        rows[1]["observed"]["loader"]["unit"].as_str().unwrap()
    );
    rows[1]["observed"]["loader"]["cgroup"] = foreign.clone().into();
    rows[3][8][0][1][1] = foreign.clone().into();
    rows[4][9][0][1][1] = foreign.into();
    Fixture::bind_rewritten_prefix(&mut rows);
    fixture.rewrite(&rows);
    let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
    assert!(
        error.to_string().contains("original actor cgroup profile"),
        "{error}"
    );
}

#[test]
fn exact_eight_attempt_bound_requires_the_complete_new_certificate() {
    let fixture = Fixture::new();
    let context = std::sync::Arc::new(fixture.context());
    let original = std::fs::read(fixture.terminal()).unwrap();
    let artifact: detcore::network_runtime::ProviderArtifact =
        serde_json::from_value(fixture.rows()[0]["artifact"].clone()).unwrap();
    // Seven real production initializations leave seven before-launch rows.
    // There is no loader, BPF operation, or synthesized success receipt.
    for index in 1..=7 {
        let root =
            crate::unix_guard_package::RecoveryDeploymentRoot::open_at(&context.accepted.path)
                .unwrap();
        let mut attempt =
            crate::accepted_terminal::AcceptedRecovery::retain(root, format!("{index:032x}"));
        attempt.set_resource_recovery(context.clone()).unwrap();
        attempt.initialize(artifact.clone()).unwrap();
    }
    let probe = || {
        let root =
            crate::unix_guard_package::RecoveryDeploymentRoot::open_at(&context.accepted.path)
                .unwrap();
        let mut attempt =
            crate::accepted_terminal::AcceptedRecovery::retain(root, fixture.label().to_owned());
        attempt.set_resource_recovery(context.clone()).unwrap();
        attempt.initialize(artifact.clone()).unwrap_err()
    };
    // The selected existing role makes an allowed admission read-only: its
    // first O_EXCL creation fails before retaining/writing any description.
    assert_eq!(probe().kind(), io::ErrorKind::AlreadyExists);
    let rows = raw_rows(&original, 5).unwrap();
    std::fs::write(
        fixture.terminal(),
        &original[..rows[0].len() + rows[1].len() + rows[2].len()],
    )
    .unwrap();
    assert_eq!(
        probe().to_string(),
        "accepted unresolved launch admission bound reached"
    );
    std::fs::write(fixture.terminal(), &original).unwrap();
    assert_eq!(probe().kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(fixture.terminal()).unwrap(), original);
    assert_eq!(
        std::fs::read_dir(&context.accepted.path).unwrap().count(),
        24
    );
}

#[test]
fn failed_child125_prefix_and_joint_wrong_abi_contract_cannot_cross_profiles() {
    let fixture = Fixture::new();
    let original = fixture.rows();
    let context = fixture.context();
    let changes: Vec<fn(&mut Vec<Value>)> = vec![
        |r| r[2]["error"] = "invented success".into(),
        |r| r[2]["stage"] = "accepted_terminal".into(),
        |r| r[2]["label"] = "f".repeat(32).into(),
        |r| r[2]["schema"] = true.into(),
        |r| {
            r[0]["artifact"]["wire_format"] = "abi11-copy5".into();
            r[1]["observed"]["artifact"]["wire_format"] = "abi11-copy5".into();
            r[0]["artifact"]["maps"] = 25.into();
            r[1]["observed"]["artifact"]["maps"] = 25.into();
            r[3][7][5][0] = "abi11-copy5".into();
            r[3][7][5][5] = 25.into();
        },
        |r| {
            r[0]["artifact"]["topology"]["contract_sha256"] = serde_json::json!(vec![7; 32]);
            r[1]["observed"]["artifact"]["topology"]["contract_sha256"] =
                serde_json::json!(vec![7; 32]);
            r[3][7][5][1] = "07".repeat(32).into();
        },
    ];
    for (index, change) in changes.into_iter().enumerate() {
        let mut rows = original.clone();
        change(&mut rows);
        Fixture::bind_rewritten_prefix(&mut rows);
        fixture.rewrite(&rows);
        let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
        let predicate = if index < 4 {
            "original failure"
        } else {
            "topology"
        };
        assert!(
            error.to_string().contains(predicate),
            "case {index}: {error}"
        );
        assert!(!context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));
    }
}

#[test]
fn failed_service_status_and_full_inventory_remain_required_with_rebound_digests() {
    let fixture = Fixture::new();
    let context = fixture.context();
    let original = fixture.rows();
    let stdout = fixture
        .directory
        .join("accepted")
        .join(format!("accepted-b1-{}.stdout.log", fixture.label()));
    let data = std::fs::read(&stdout).unwrap();
    let service: Vec<Value> = raw_rows(&data, 2)
        .unwrap()
        .iter()
        .map(|row| serde_json::from_slice(row).unwrap())
        .collect();
    let changes: Vec<fn(&mut Vec<Value>)> = vec![
        |r| r[1]["service_status"] = 0.into(),
        |r| r[1]["close_receipts"][0]["inventory"]["ids"][0]["id"] = 999.into(),
        |r| r[0]["inventories"][0]["complete"] = false.into(),
        |r| r[1]["close_receipts"][0]["close"]["returned"] = (-1).into(),
    ];
    let predicates = [
        "close shape",
        "closed inventory",
        "inventory is incomplete",
        "close operation",
    ];
    for (index, (change, predicate)) in changes.into_iter().zip(predicates).enumerate() {
        let mut altered = service.clone();
        change(&mut altered);
        let mut bytes = Vec::new();
        for row in &altered {
            bytes.extend(serde_json::to_vec(row).unwrap());
            bytes.push(b'\n');
        }
        std::fs::write(&stdout, &bytes).unwrap();
        let stat = file_stat(&stdout.metadata().unwrap());
        let mut rows = original.clone();
        rows[3][7][4][1][6] = serde_json::json!(bytes.len());
        rows[3][7][4][1][7] = stat[6].clone();
        rows[3][7][4][1][8] = stat[7].clone();
        rows[3][7][4][1][9] = digest(&bytes).into();
        let mut intent = serde_json::to_vec(&rows[3]).unwrap();
        intent.push(b'\n');
        rows[4][1] = digest(&intent).into();
        fixture.rewrite(&rows);
        let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
        assert!(
            error.to_string().contains(predicate),
            "case {index}: {error}"
        );
    }
}

#[test]
fn child125_certificate_discharges_one_debt_at_the_unchanged_eight_attempt_bound() {
    let fixture = Fixture::new();
    let context = std::sync::Arc::new(fixture.context());
    let original = std::fs::read(fixture.terminal()).unwrap();
    let rows = raw_rows(&original, 5).unwrap();
    let before: Value = serde_json::from_slice(rows[0]).unwrap();
    let failed: Value = serde_json::from_slice(rows[2]).unwrap();
    let artifact: detcore::network_runtime::ProviderArtifact =
        serde_json::from_value(before["artifact"].clone()).unwrap();
    assert_eq!(before["artifact"]["wire_format"], "abi12-copy5");
    assert_eq!(failed["stage"], "accepted_failed");
    assert_eq!(
        failed["error"],
        "accepted terminal: accepted service wrapper failed exit status: 125; raw logs retained; child_wait=Some(32000)"
    );
    assert_eq!(fixture.metadata["result"]["execution_success"], false);
    assert_eq!(fixture.metadata["result"]["normal_terminal_success"], false);
    let root =
        crate::unix_guard_package::RecoveryDeploymentRoot::open_at(&context.accepted.path).unwrap();
    assert!(
        crate::accepted_terminal::validate_accepted_receipt(&root, fixture.label(), &artifact)
            .is_err()
    );

    // These seven attempts use the actual initialization/admission path. There
    // is no loader, BPF operation, missing wait, or invented normal terminal.
    for index in 1..=7 {
        let root =
            crate::unix_guard_package::RecoveryDeploymentRoot::open_at(&context.accepted.path)
                .unwrap();
        let mut attempt =
            crate::accepted_terminal::AcceptedRecovery::retain(root, format!("{index:032x}"));
        attempt.set_resource_recovery(context.clone()).unwrap();
        attempt.initialize(artifact.clone()).unwrap();
    }
    let snapshot = || {
        let mut files = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(&context.accepted.path).unwrap() {
            let path = entry.unwrap().path();
            files.insert(
                path.file_name().unwrap().to_owned(),
                (
                    file_stat(&path.metadata().unwrap()),
                    std::fs::read(path).unwrap(),
                ),
            );
        }
        files
    };
    let before_probe = snapshot();
    assert_eq!(before_probe.len(), 24);
    let root =
        crate::unix_guard_package::RecoveryDeploymentRoot::open_at(&context.accepted.path).unwrap();
    let mut probe =
        crate::accepted_terminal::AcceptedRecovery::retain(root, fixture.label().to_owned());
    probe.set_resource_recovery(context.clone()).unwrap();
    let error = probe.initialize(artifact).unwrap_err();
    // Passing admission must reach the original O_EXCL failure on the selected
    // existing label. It must not create, delete, or rewrite any retained role.
    assert_eq!(
        error.kind(),
        io::ErrorKind::AlreadyExists,
        "ABI12 child125 certificate must discharge exactly one resource debt: {error}"
    );
    assert_eq!(snapshot(), before_probe);
    assert_eq!(std::fs::read(fixture.terminal()).unwrap(), original);
}

#[test]
fn child_wait_zero_signal_missing_and_other_statuses_cannot_enter_child125_profile() {
    let fixture = Fixture::new();
    let context = fixture.context();
    let original = fixture.rows();
    for wait in ["Some(0)", "Some(9)", "Some(256)", "None", "Some(32512)"] {
        let mut rows = original.clone();
        rows[2]["error"] = format!(
            "accepted terminal: accepted service wrapper failed exit status: 125; raw logs retained; child_wait={wait}"
        ).into();
        Fixture::bind_rewritten_prefix(&mut rows);
        fixture.rewrite(&rows);
        let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
        assert!(
            error.to_string().contains("original failure"),
            "{wait}: {error}"
        );
        assert!(!context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));
    }
    fixture.rewrite(&original);
    resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap();
}

#[test]
fn explicit_null_original_service_fields_remain_required_after_rebinding() {
    let fixture = Fixture::new();
    let context = fixture.context();
    let original = fixture.rows();
    let stdout = fixture
        .directory
        .join("accepted")
        .join(format!("accepted-b1-{}.stdout.log", fixture.label()));
    let data = std::fs::read(&stdout).unwrap();
    let service: Vec<Value> = raw_rows(&data, 2)
        .unwrap()
        .iter()
        .map(|row| serde_json::from_slice(row).unwrap())
        .collect();
    assert_eq!(service[0]["provider"]["active_setters"], 1);
    assert_eq!(service[0]["fd_journal_unretired"], 322);
    for (index, key) in [(0, "failure"), (1, "close_error")] {
        for missing in [true, false] {
            let mut altered = service.clone();
            if missing {
                altered[index].as_object_mut().unwrap().remove(key);
            } else {
                altered[index][key] = "actual physical-close failure".into();
            }
            let mut bytes = Vec::new();
            for row in &altered {
                bytes.extend(serde_json::to_vec(row).unwrap());
                bytes.push(b'\n');
            }
            std::fs::write(&stdout, &bytes).unwrap();
            let stat = file_stat(&stdout.metadata().unwrap());
            let mut rows = original.clone();
            rows[3][7][4][1][6] = serde_json::json!(bytes.len());
            rows[3][7][4][1][7] = stat[6].clone();
            rows[3][7][4][1][8] = stat[7].clone();
            rows[3][7][4][1][9] = digest(&bytes).into();
            let mut intent = serde_json::to_vec(&rows[3]).unwrap();
            intent.push(b'\n');
            rows[4][1] = digest(&intent).into();
            fixture.rewrite(&rows);
            let error =
                resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
            assert_eq!(
                error.to_string(),
                "accepted service transcript lacks explicit clean error fields",
                "{key}/{missing}: {error}"
            );
            assert!(!context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));
        }
    }
}
