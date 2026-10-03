use super::*;

struct Fixture {
    _temporary: tempfile::TempDir,
    context: ResourceRecoveryContext,
    label: String,
    terminal: [PathBuf; 2],
    prefix: [Vec<u8>; 2],
    intent: Value,
    result: Value,
}
impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path().join("fixture");
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/test_network_recovery.py");
        let output = std::process::Command::new("python3")
            .arg("-B")
            .arg(script)
            .arg("--fixture")
            .arg(&base)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(metadata["native_recovery_evidence"], false);
        let accepted = Path::new(metadata["accepted_root"].as_str().unwrap());
        let unix = Path::new(metadata["unix_root"].as_str().unwrap());
        let pins = Path::new(metadata["pin_root"].as_str().unwrap());
        let context = ResourceRecoveryContext::open(accepted, unix, pins).unwrap();
        let label = metadata["accepted_label"].as_str().unwrap().to_owned();
        let terminal = [
            accepted.join(format!("accepted-b1-{label}.terminal.jsonl")),
            unix.join(format!("guard-b1-{label}.terminal.jsonl")),
        ];
        let raw = terminal.each_ref().map(|p| std::fs::read(p).unwrap());
        let rows = [raw_rows(&raw[0], 5).unwrap(), raw_rows(&raw[1], 4).unwrap()];
        let prefix = [rows[0][..3].concat(), rows[1][..2].concat()];
        let intent = serde_json::from_slice(rows[0][3]).unwrap();
        let result = serde_json::from_slice(rows[0][4]).unwrap();
        Self {
            _temporary: temporary,
            context,
            label,
            terminal,
            prefix,
            intent,
            result,
        }
    }
    fn accepts(&self) -> io::Result<()> {
        self.context
            .resolve(0, self.context.accepted.file.as_fd(), &self.label)?;
        self.context
            .resolve(1, self.context.unix.file.as_fd(), &self.label)
    }
    fn rewrite(&self, mut intent: Value, mut result: Value) {
        intent[10] = digest(&serde_json::to_vec(&intent[9]).unwrap()).into();
        let mut i = serde_json::to_vec(&intent).unwrap();
        i.push(b'\n');
        result[1] = digest(&i).into();
        let mut r = serde_json::to_vec(&result).unwrap();
        r.push(b'\n');
        for (path, prefix) in self.terminal.iter().zip(&self.prefix) {
            std::fs::write(path, [prefix.as_slice(), &i, &r].concat()).unwrap();
        }
    }
}

#[test]
fn resource_pair_consumes_actual_writer_rows_without_relabelling_original_failure() {
    let fixture = Fixture::new();
    fixture.accepts().unwrap();
    assert!(
        fixture
            .context
            .accepted_resolved(fixture.context.accepted.file.as_fd(), &fixture.label)
    );
    assert!(fixture.context.unix_resolved(
        fixture.context.unix.file.as_fd(),
        fixture.context.pins.file.as_fd(),
        &fixture.label
    ));
    let root =
        crate::unix_guard_package::RecoveryDeploymentRoot::open_at(&fixture.context.accepted.path)
            .unwrap();
    let first: Value = serde_json::from_slice(raw_rows(&fixture.prefix[0], 3).unwrap()[0]).unwrap();
    let artifact: detcore::network_runtime::ProviderArtifact =
        serde_json::from_value(first["artifact"].clone()).unwrap();
    assert!(
        crate::accepted_terminal::validate_accepted_receipt(&root, &fixture.label, &artifact)
            .is_err()
    );
    assert!(
        fixture.prefix[0]
            .windows(b"accepted_failed".len())
            .any(|v| v == b"accepted_failed")
    );
    assert!(
        fixture.prefix[1]
            .windows(b"terminal_failed".len())
            .any(|v| v == b"terminal_failed")
    );
}

#[test]
fn resource_pair_refuses_changed_bindings_and_incomplete_or_false_absence() {
    let fixture = Fixture::new();
    fixture.accepts().unwrap();
    for variant in 0..43 {
        let mut i = fixture.intent.clone();
        let mut r = fixture.result.clone();
        match variant {
            0 => {
                i[5] = "a".repeat(64).into();
                r[11] = i[5].clone();
            }
            1 => i[6][0] = "/unconfigured/root".into(),
            2 => i[6][1][1] = 0.into(),
            3 => i[4] = (i[4].as_u64().unwrap() ^ 1).into(),
            4 => i[6][3] = "ef".repeat(16).into(),
            5 => i[6][5][2] = "ff".repeat(32).into(),
            6 => {
                i[8][0][2][0] = "ab".repeat(16).into();
                r[10] = i[8].clone();
            }
            7 => {
                i[1] = "00000000-0000-0000-0000-000000000000".into();
                r[2] = i[1].clone();
            }
            8 => i[6][4][0][6] = 1.into(),
            9 => i[6][4][0][9] = "ff".repeat(32).into(),
            10 => i[6][4][1][7] = 0.into(),
            11 => i[9][0][0] = 1.into(),
            12 => i[9][0][2] = 0.into(),
            13 => {
                i[9].as_array_mut().unwrap().pop();
            }
            14 => i[9][1] = i[9][0].clone(),
            15 => {
                r[3].as_array_mut().unwrap().pop();
            }
            16 => r[3][1] = r[3][0].clone(),
            17 => r[3][0][2] = 0.into(),
            18 => r[3][0][1] = 0.into(),
            19 => r[3][0][8] = 1.into(),
            20 => r[3][0][9] = (libc::EIO as u64).into(),
            21 => r[3][0][4] = 0.into(),
            22 => r[3][41][0] = "wrong-directory".into(),
            23 => r[3].as_array_mut().unwrap().swap(0, 1),
            24 => r[3][0][7] = u64::MAX.into(),
            25 => {
                r[8][0][2].as_array_mut().unwrap().pop();
            }
            26 => r[8][0][2][1] = r[8][0][2][0].clone(),
            27 => r[8][0][2][0][0] = 1.into(),
            28 => r[8][0][2][0][3] = (libc::EPERM as u64).into(),
            29 => r[8][0][2][0][3] = 0.into(),
            30 => r[8][0][2][0][2] = 0.into(),
            31 => {
                r[8].as_array_mut().unwrap().pop();
            }
            32 => r[7] = 256.into(),
            33 => r[6][0] = 0.into(),
            34 => r[6][2] = 1.into(),
            35 => r[4] = 0.into(),
            36 => r[5] = 0.into(),
            37 => {
                r[4] = u64::MAX.into();
                r[5] = 0.into();
            }
            38 => r[9] = u64::MAX.into(),
            39 => r[8][1][0] = 0.into(),
            40 => r[10][2][0] = "replaced.service".into(),
            41 => {
                i[7][4][5][0][1] = 999999.into();
            }
            42 => {
                i[3] = u64::MAX.into();
            }
            _ => unreachable!(),
        }
        fixture.rewrite(i, r);
        assert!(
            fixture.accepts().is_err(),
            "accepted corrupt resource variant {variant}"
        );
    }
    fixture.rewrite(fixture.intent.clone(), fixture.result.clone());
    fixture.accepts().unwrap();
}

#[test]
fn resource_pair_requires_literal_complete_peer_rows_and_unchanged_original_files() {
    let fixture = Fixture::new();
    fixture.accepts().unwrap();
    let original = fixture
        .terminal
        .each_ref()
        .map(|p| std::fs::read(p).unwrap());
    for variant in 0..6 {
        for (path, bytes) in fixture.terminal.iter().zip(&original) {
            std::fs::write(path, bytes).unwrap();
        }
        match variant {
            0 => {
                std::fs::write(&fixture.terminal[1], &fixture.prefix[1]).unwrap();
            }
            1 => {
                let rows = raw_rows(&original[1], 4).unwrap();
                std::fs::write(&fixture.terminal[1], rows[..3].concat()).unwrap();
            }
            2 => {
                std::fs::write(&fixture.terminal[1], &original[1][..original[1].len() - 1])
                    .unwrap();
            }
            3 => {
                let mut b = original[1].clone();
                b.extend_from_slice(b"[]\n");
                std::fs::write(&fixture.terminal[1], b).unwrap();
            }
            4 => {
                let rows = raw_rows(&original[1], 4).unwrap();
                std::fs::write(
                    &fixture.terminal[1],
                    [rows[0], rows[1], b" ", rows[2], rows[3]].concat(),
                )
                .unwrap();
            }
            5 => {
                let mut b = original[0].clone();
                let at = b.iter().position(|c| *c == b'a').unwrap();
                b[at] = b'b';
                std::fs::write(&fixture.terminal[0], b).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            fixture.accepts().is_err(),
            "accepted incomplete/mismatched peer variant {variant}"
        );
    }
    for (path, bytes) in fixture.terminal.iter().zip(&original) {
        std::fs::write(path, bytes).unwrap();
    }
    fixture.accepts().unwrap();
    let stdout = fixture
        .context
        .accepted
        .path
        .join(format!("accepted-b1-{}.stdout.log", fixture.label));
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(stdout)
        .unwrap();
    assert!(fixture.accepts().is_err());
}

#[test]
fn resource_root_lock_lives_with_original_description_and_refuses_exclusive_recovery() {
    // A parallel fixture's fork would temporarily inherit this description,
    // invalidating the last-owner premise. Keep every assertion unchanged in
    // the existing isolated, reaped test process; do not poll past a failed lock.
    if crate::unix_guard_terminal::isolate_command_parent(
        "network_recovery::tests::resource_root_lock_lives_with_original_description_and_refuses_exclusive_recovery",
        false,
    ) {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let original = Root::open(temp.path()).unwrap();
    let alias = original.file.try_clone().unwrap();
    let separate = File::open(temp.path()).unwrap();
    assert_eq!(
        unsafe { libc::flock(separate.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::EWOULDBLOCK)
    );
    drop(original);
    assert_eq!(
        unsafe { libc::flock(separate.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        -1
    );
    drop(alias);
    assert_eq!(
        unsafe { libc::flock(separate.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(
        Root::open(temp.path()).is_err(),
        "launch admitted during exclusive recovery lock"
    );
}

#[test]
fn resource_pair_preserves_original_nonzero_artifact_requirements_on_joint_mutation() {
    let mut fixture = Fixture::new();
    fixture.accepts().unwrap();
    let original_prefix = fixture.prefix[0].clone();
    for (pointer, certificate_index) in [
        ("/topology/contract_sha256", 1),
        ("/object_sha256", 2),
        ("/library_sha256", 3),
        ("/btf_sha256", 4),
    ] {
        let mut rows: Vec<Value> = raw_rows(&original_prefix, 3)
            .unwrap()
            .into_iter()
            .map(|row| serde_json::from_slice(row).unwrap())
            .collect();
        *rows[0]["artifact"].pointer_mut(pointer).unwrap() =
            serde_json::to_value([0u8; 32]).unwrap();
        rows[1]["observed"]["artifact"] = rows[0]["artifact"].clone();
        fixture.prefix[0] = rows
            .into_iter()
            .flat_map(|row| {
                let mut raw = serde_json::to_vec(&row).unwrap();
                raw.push(b'\n');
                raw
            })
            .collect();
        let mut intent = fixture.intent.clone();
        intent[6][5][certificate_index] = "00".repeat(32).into();
        intent[6][4][0][6] = (fixture.prefix[0].len() as u64).into();
        intent[6][4][0][9] = digest(&fixture.prefix[0]).into();
        fixture.rewrite(intent, fixture.result.clone());
        let error = fixture.accepts().unwrap_err().to_string();
        assert!(
            error.contains("artifact digest is zero"),
            "{pointer}: {error}"
        );
    }
    fixture.prefix[0] = original_prefix;
    fixture.rewrite(fixture.intent.clone(), fixture.result.clone());
    fixture.accepts().unwrap();
}

#[test]
fn resource_admission_keeps_the_eight_attempt_bound_and_only_discharges_the_proven_pair() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.accepts().unwrap();
    let root_path = fixture.context.accepted.path.clone();
    // Seven other unresolved attempts plus the original resource failure hit
    // the unchanged bound. These deliberately incomplete receipts grant no
    // authority and can never be discharged by the selected pair's proof.
    for ordinal in 1..=7 {
        for role in ["terminal.jsonl", "stdout.log", "stderr.log"] {
            let path = root_path.join(format!("accepted-b1-{ordinal:032x}.{role}"));
            std::fs::write(&path, b"retained unresolved attempt\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    let before: Value =
        serde_json::from_slice(raw_rows(&fixture.prefix[0], 3).unwrap()[0]).unwrap();
    let artifact: detcore::network_runtime::ProviderArtifact =
        serde_json::from_value(before["artifact"].clone()).unwrap();
    let open = || crate::unix_guard_package::RecoveryDeploymentRoot::open_at(&root_path).unwrap();
    let mut refused = crate::accepted_terminal::AcceptedRecovery::retain(open(), "ee".repeat(16));
    assert!(
        refused
            .initialize(artifact.clone())
            .unwrap_err()
            .to_string()
            .contains("unresolved launch admission bound")
    );
    let original = fixture
        .terminal
        .each_ref()
        .map(|path| std::fs::read(path).unwrap());
    let context = std::sync::Arc::new(fixture.context);
    let mut admitted = crate::accepted_terminal::AcceptedRecovery::retain(open(), "ef".repeat(16));
    admitted.set_resource_recovery(context.clone()).unwrap();
    admitted.initialize(artifact.clone()).unwrap();
    for (path, bytes) in fixture.terminal.iter().zip(original) {
        assert_eq!(
            std::fs::read(path).unwrap(),
            bytes,
            "admission rewrote original failure evidence"
        );
    }
    // The new incomplete launch occupies the eighth slot again. Nothing is
    // removed or reclassified merely because another pair proved absence.
    let mut bounded = crate::accepted_terminal::AcceptedRecovery::retain(open(), "ed".repeat(16));
    bounded.set_resource_recovery(context).unwrap();
    assert!(
        bounded
            .initialize(artifact)
            .unwrap_err()
            .to_string()
            .contains("unresolved launch admission bound")
    );
}
