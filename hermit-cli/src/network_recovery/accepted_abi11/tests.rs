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
            .join("../scripts/test_accepted_resource_abi11_recovery.py");
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
        let rows = raw_rows(&bytes, 4).unwrap();
        let prefix = bytes[..rows[0].len() + rows[1].len()].to_vec();
        let prefix_values = rows[..2]
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
        raw_rows(&std::fs::read(self.terminal()).unwrap(), 4)
            .unwrap()
            .into_iter()
            .map(|row| serde_json::from_slice(row).unwrap())
            .collect()
    }
    fn rewrite(&self, rows: &[Value]) {
        let original_prefix = rows.len() >= 2 && rows[..2] == self.prefix_values[..];
        let mut bytes = if original_prefix {
            self.prefix.clone()
        } else {
            Vec::new()
        };
        for value in rows.iter().skip(if original_prefix { 2 } else { 0 }) {
            bytes.extend(serde_json::to_vec(value).unwrap());
            bytes.push(b'\n');
        }
        std::fs::write(self.terminal(), bytes).unwrap();
    }
    fn bind_rewritten_prefix(rows: &mut [Value]) {
        let mut prefix = Vec::new();
        for row in &rows[..2] {
            prefix.extend(serde_json::to_vec(row).unwrap());
            prefix.push(b'\n');
        }
        rows[2][7][4][0][6] = serde_json::json!(prefix.len());
        rows[2][7][4][0][9] = digest(&prefix).into();
        let mut intent = serde_json::to_vec(&rows[2]).unwrap();
        intent.push(b'\n');
        rows[3][1] = digest(&intent).into();
    }
}

#[test]
fn actual_abi11_writer_certificate_is_resource_only_and_preserves_old_resolvers() {
    let fixture = Fixture::new();
    let context = fixture.context();
    resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap();
    assert!(context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));
    assert!(
        context
            .resolve(0, context.accepted.file.as_fd(), fixture.label())
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
    assert_eq!(fixture.rows()[2][9].as_array().unwrap().len(), 123);
    assert_eq!(fixture.rows()[2][7][5][0], "abi11-copy5");
    assert_eq!(fixture.rows()[2][7][5][5], 25);
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
        |r| r[2][0] = "unknown-intent".into(),
        |r| r[3][0] = "hermit-failed-resource-result-v1".into(),
        |r| r[2][1] = "00000000-0000-0000-0000-000000000000".into(),
        |r| r[2][4] = serde_json::json!(u32::MAX),
        |r| r[2][5] = "0".repeat(64).into(),
        |r| r[2][6] = "0".repeat(64).into(),
        |r| r[2][7][0] = "/wrong-root".into(),
        |r| r[2][7][1][1] = 0.into(),
        |r| r[2][7][2] = "f".repeat(32).into(),
        |r| r[2][7][3] = "f".repeat(32).into(),
        |r| r[2][7][4][1][9] = "0".repeat(64).into(),
        |r| r[2][7][5][6] = 23.into(),
        |r| r[2][7][6] = 0.into(),
        |r| r[2][8][0][1][0] = "f".repeat(32).into(),
        |r| r[2][8][0][2][0][1] = "active".into(),
        |r| r[2][8][0][3] = 1.into(),
        |r| r[2][8][0][6] = libc::EACCES.into(),
        |r| {
            r[2][9].as_array_mut().unwrap().pop();
        },
        |r| r[2][9][1] = r[2][9][0].clone(),
        |r| r[2][9][0][0] = 1.into(),
        |r| r[2][10] = "0".repeat(64).into(),
        |r| r[2][11] = "normal-success".into(),
        |r| r[3][1] = "0".repeat(64).into(),
        |r| r[3][3] = 0.into(),
        |r| r[3][4] = u64::MAX.into(),
        |r| r[3][5][0] = 0.into(),
        |r| r[3][5][1] = 0.into(),
        |r| r[3][5][2] = 1.into(),
        |r| r[3][5][3] = 1.into(),
        |r| r[3][5][4] = "0".repeat(64).into(),
        |r| r[3][5][5] = "0".repeat(64).into(),
        |r| r[3][6] = 125.into(),
        |r| {
            r[3][7].as_array_mut().unwrap().pop();
        },
        |r| {
            r[3][7][0][2].as_array_mut().unwrap().pop();
        },
        |r| r[3][7][0][2][1] = r[3][7][0][2][0].clone(),
        |r| r[3][7][1][2][0][3] = libc::EPERM.into(),
        |r| r[3][7][1][2][0][3] = 0.into(),
        |r| r[3][7][1][0] = 0.into(),
        |r| r[3][8] = u64::MAX.into(),
        |r| r[3][9][1][1][3] = 0.into(),
        |r| r[3][9][0][4] = 0.into(),
        |r| r[3][10] = "0".repeat(64).into(),
        |r| r[3][11] = "0".repeat(64).into(),
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
            let mut intent = serde_json::to_vec(&rows[2]).unwrap();
            intent.push(b'\n');
            rows[3][1] = digest(&intent).into();
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
    for count in 2..=3 {
        fixture.rewrite(&original[..count]);
        assert!(!context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));
    }
    let mut extra = original.clone();
    extra.push(original[3].clone());
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
    rows[3][1] = "0".repeat(64).into();
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
    rows[2][8][0][1][1] = foreign.clone().into();
    rows[3][9][0][1][1] = foreign.into();
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
    let rows = raw_rows(&original, 4).unwrap();
    std::fs::write(
        fixture.terminal(),
        &original[..rows[0].len() + rows[1].len()],
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

impl Fixture {
    fn bind_intent(rows: &mut [Value]) {
        let mut intent = serde_json::to_vec(&rows[2]).unwrap();
        intent.push(b'\n');
        rows[3][1] = digest(&intent).into();
    }

    fn service(&self) -> Vec<Value> {
        let path = self
            .directory
            .join("accepted")
            .join(format!("accepted-b1-{}.stdout.log", self.label()));
        raw_rows(&std::fs::read(path).unwrap(), 2)
            .unwrap()
            .into_iter()
            .map(|row| serde_json::from_slice(row).unwrap())
            .collect()
    }

    fn rewrite_service_and_bind(&self, service: &[Value], rows: &mut [Value]) {
        let path = self
            .directory
            .join("accepted")
            .join(format!("accepted-b1-{}.stdout.log", self.label()));
        let mut bytes = Vec::new();
        for row in service {
            bytes.extend(serde_json::to_vec(row).unwrap());
            bytes.push(b'\n');
        }
        std::fs::write(&path, &bytes).unwrap();
        let stat = file_stat(&path.metadata().unwrap());
        let proof = &mut rows[2][7][4][1];
        proof[6] = serde_json::json!(bytes.len());
        proof[7] = stat[6].clone();
        proof[8] = stat[7].clone();
        proof[9] = digest(&bytes).into();
        Self::bind_intent(rows);
    }
}

#[test]
fn old_tags_and_joint_abi_or_contract_relabelling_never_admit_abi11() {
    let fixture = Fixture::new();
    let original = fixture.rows();
    let context = fixture.context();
    for (intent, result) in [
        (
            "hermit-accepted-resource-intent-v1",
            "hermit-accepted-resource-result-v1",
        ),
        (
            "hermit-accepted-failed-abi8-resource-intent-v1",
            "hermit-accepted-failed-abi8-resource-result-v1",
        ),
    ] {
        let mut rows = original.clone();
        rows[2][0] = intent.into();
        rows[3][0] = result.into();
        Fixture::bind_intent(&mut rows);
        fixture.rewrite(&rows);
        let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
        assert!(error.to_string().contains("protocol differs"), "{error}");
        assert!(!context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));
    }
    for wire in ["abi9-copy5", "abi8-copy5", "abi11-copy4"] {
        let mut rows = original.clone();
        for artifact in [0, 1] {
            let a = if artifact == 0 {
                &mut rows[0]["artifact"]
            } else {
                &mut rows[1]["observed"]["artifact"]
            };
            a["wire_format"] = wire.into();
            if wire != "abi11-copy4" {
                a["maps"] = 24.into();
            }
        }
        rows[2][7][5][0] = wire.into();
        if wire != "abi11-copy4" {
            rows[2][7][5][5] = 24.into();
        }
        Fixture::bind_rewritten_prefix(&mut rows);
        fixture.rewrite(&rows);
        let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
        assert!(
            error.to_string().contains("source topology differs"),
            "{wire}: {error}"
        );
        assert!(!context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));
    }
    let mut rows = original.clone();
    for index in 0..2 {
        let a = if index == 0 {
            &mut rows[0]["artifact"]
        } else {
            &mut rows[1]["observed"]["artifact"]
        };
        a["topology"]["contract_sha256"] = serde_json::json!(vec![1; 32]);
    }
    rows[2][7][5][1] = "01".repeat(32).into();
    Fixture::bind_rewritten_prefix(&mut rows);
    fixture.rewrite(&rows);
    let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
    assert!(
        error.to_string().contains("source topology differs"),
        "{error}"
    );
    fixture.rewrite(&original);
    resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap();
}

#[test]
fn original_123_id_population_cannot_be_changed_even_with_rebound_complete_scans() {
    let fixture = Fixture::new();
    let context = fixture.context();
    let original = fixture.rows();
    let service = fixture.service();
    for case in 0..4 {
        let mut changed = service.clone();
        let ids = changed[0]["inventories"][0]["ids"].as_array_mut().unwrap();
        let map = ids.iter().position(|id| id["kind"] == 0).unwrap();
        match case {
            0 => {
                ids.remove(map);
            }
            1 => {
                ids[map] = ids[(map + 1) % ids.len()].clone();
            }
            2 => {
                ids.push(serde_json::json!({"kind":0,"id":u32::MAX}));
            }
            3 => {
                ids[map] = serde_json::json!({"kind":1,"id":u32::MAX});
            }
            _ => unreachable!(),
        }
        let ids = ids.clone();
        changed[1]["close_receipts"][0]["inventory"]["ids"] = ids.clone().into();
        let mut triples: Vec<(u64, u64)> = ids
            .iter()
            .map(|id| (id["kind"].as_u64().unwrap(), id["id"].as_u64().unwrap()))
            .collect();
        triples.sort_unstable();
        let mut rows = original.clone();
        rows[2][9] = triples
            .iter()
            .map(|&(kind, id)| serde_json::json!([0, kind, id]))
            .collect::<Vec<_>>()
            .into();
        rows[2][10] = digest(&serde_json::to_vec(&rows[2][9]).unwrap()).into();
        for scan in rows[3][7].as_array_mut().unwrap() {
            scan[2] = triples
                .iter()
                .map(|&(kind, id)| serde_json::json!([0, kind, id, libc::ENOENT]))
                .collect::<Vec<_>>()
                .into();
        }
        fixture.rewrite_service_and_bind(&changed, &mut rows);
        fixture.rewrite(&rows);
        let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
        let predicate = if case == 1 {
            "duplicate or invalid accepted original ID"
        } else {
            "ID inventory differs from authenticated artifact"
        };
        assert!(
            error.to_string().contains(predicate),
            "case {case}: {error}"
        );
        assert!(!context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));
    }
    let mut restored = original;
    fixture.rewrite_service_and_bind(&service, &mut restored);
    fixture.rewrite(&restored);
    resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap();
}

#[test]
fn failed_service_and_renewed_or_extended_deadlines_remain_refused() {
    let fixture = Fixture::new();
    let context = fixture.context();
    let original = fixture.rows();
    let service = fixture.service();
    let mut failed = service.clone();
    failed[1]["service_status"] = 125.into();
    let mut rows = original.clone();
    fixture.rewrite_service_and_bind(&failed, &mut rows);
    fixture.rewrite(&rows);
    let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("did not complete its original single load"),
        "{error}"
    );
    assert!(!context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));

    let mut restored = original;
    fixture.rewrite_service_and_bind(&service, &mut restored);
    fixture.rewrite(&restored);
    resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap();
    for case in 0..3 {
        let mut rows = restored.clone();
        match case {
            0 => rows[2][3] = (rows[2][3].as_u64().unwrap() + 1).into(),
            1 => rows[3][4] = (rows[3][4].as_u64().unwrap() + 1).into(),
            2 => rows[2][2] = rows[2][7][6].clone(),
            _ => unreachable!(),
        }
        Fixture::bind_intent(&mut rows);
        fixture.rewrite(&rows);
        let error = resolve(&context, context.accepted.file.as_fd(), fixture.label()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("fresh verification interval differs"),
            "case {case}: {error}"
        );
        assert!(!context.accepted_resolved(context.accepted.file.as_fd(), fixture.label()));
    }
}
