use super::*;
use std::collections::VecDeque;

type ProofHook = Option<(u64, Box<dyn FnOnce() + Send>)>;

struct OfflineClient {
    expected: CustomProcessIdentity,
    liveness: Mutex<VecDeque<ProcessLiveness>>,
    liveness_calls: AtomicU64,
    rpc_calls: AtomicU64,
    proof_hook: Mutex<ProofHook>,
    probe_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl OfflineClient {
    fn new(expected: &CustomProcessIdentity, states: &[ProcessLiveness]) -> Self {
        Self {
            expected: expected.clone(),
            liveness: Mutex::new(states.iter().copied().collect()),
            liveness_calls: AtomicU64::new(0),
            rpc_calls: AtomicU64::new(0),
            proof_hook: Mutex::new(None),
            probe_hook: Mutex::new(None),
        }
    }

    fn unavailable<T>(&self) -> AdapterResult<T> {
        self.rpc_calls.fetch_add(1, Ordering::Relaxed);
        if let Some(hook) = self.probe_hook.lock().unwrap().take() {
            hook();
        }
        Err(AdapterError::unavailable("terminal server is unavailable"))
    }
}

impl AgentApi for OfflineClient {
    fn panes(&self) -> AdapterResult<Vec<Pane>> {
        self.unavailable()
    }
    fn pane_info(&self, _: &str) -> AdapterResult<AgentPaneInfo> {
        self.unavailable()
    }
    fn workspace_label(&self, _: &str) -> AdapterResult<String> {
        self.unavailable()
    }
    fn run(&self, _: &str, _: &str) -> AdapterResult<()> {
        self.unavailable()
    }
    fn wait_agent_status(&self, _: &str, _: &str, _: u64) -> AdapterResult<()> {
        self.unavailable()
    }
    fn read(&self, _: &str, _: &str, _: Option<usize>) -> AdapterResult<String> {
        self.unavailable()
    }
}

impl ManagedApi for OfflineClient {
    fn process_liveness(&self, expected: &CustomProcessIdentity) -> ProcessLiveness {
        assert_eq!(
            expected, &self.expected,
            "the proof must use the saved harness pin"
        );
        let call = self.liveness_calls.fetch_add(1, Ordering::Relaxed) + 1;
        let mut hook = self.proof_hook.lock().unwrap();
        if hook.as_ref().is_some_and(|(expected, _)| *expected == call) {
            hook.take().unwrap().1();
        }
        let mut states = self.liveness.lock().unwrap();
        if states.len() > 1 {
            states.pop_front().unwrap()
        } else {
            *states.front().unwrap()
        }
    }
    fn workspace_id_for_label(&self, _: &str) -> AdapterResult<Option<String>> {
        self.unavailable()
    }
    fn create_workspace(
        &self,
        _: &str,
        _: &str,
        _: &[String],
    ) -> AdapterResult<(String, String, String)> {
        self.unavailable()
    }
    fn create_tab(&self, _: &str, _: &str, _: &str, _: &[String]) -> AdapterResult<String> {
        self.unavailable()
    }
    fn create_tab_with_pane(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &[String],
    ) -> AdapterResult<(String, String)> {
        self.unavailable()
    }
    fn close_pane(&self, _: &str) -> AdapterResult<()> {
        self.unavailable()
    }
    fn focus_pane(&self, _: &str) -> AdapterResult<()> {
        self.unavailable()
    }
    fn rename_tab(&self, _: &str, _: &str) -> AdapterResult<()> {
        self.unavailable()
    }
    fn start_agent(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &[String],
        _: Duration,
    ) -> AdapterResult<()> {
        self.unavailable()
    }
    fn agent_pane(&self, _: &str) -> AdapterResult<String> {
        self.unavailable()
    }
    fn report_agent_session(&self, _: &str, _: &str, _: &str, _: &str) -> AdapterResult<()> {
        self.unavailable()
    }
    fn send_keys(&self, _: &str, _: &str) -> AdapterResult<()> {
        self.unavailable()
    }
    fn close_tab(&self, _: &str) -> AdapterResult<()> {
        self.unavailable()
    }
}

fn adopted() -> (Fixture, AgentRecord) {
    let fixture = Fixture::new();
    fixture.adopt();
    let record = fixture.manager().load("foreign").unwrap();
    (fixture, record)
}

fn options(fixture: &Fixture, record: &AgentRecord) -> StopOptions {
    StopOptions {
        expected_token: Some(record.token.clone()),
        expected_record_sha256: Some(format!(
            "{:x}",
            Sha256::digest(fs::read(fixture.root.join("registry/foreign/agent.json")).unwrap())
        )),
        retire_dead_adoption: true,
        ..StopOptions::default()
    }
}

fn offline_manager<'a>(
    fixture: &Fixture,
    client: &'a OfflineClient,
) -> ManagedAgents<'a, OfflineClient> {
    ManagedAgents::new(client, &fixture.root.join("registry")).unwrap()
}

type ArtifactSnapshot = BTreeMap<PathBuf, (u64, u64, u32, Option<Vec<u8>>)>;

fn artifacts(root: &Path) -> ArtifactSnapshot {
    fn visit(root: &Path, path: &Path, files: &mut ArtifactSnapshot) {
        let metadata = fs::symlink_metadata(path).unwrap();
        files.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            (
                metadata.dev(),
                metadata.ino(),
                metadata.mode(),
                metadata.is_file().then(|| fs::read(path).unwrap()),
            ),
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), files);
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

#[test]
fn dead_adoption_retirement_preserves_every_original_artifact_without_server_calls() {
    let (fixture, record) = adopted();
    let directory = fixture.root.join("registry/foreign");
    let path = directory.join("agent.json");
    let mut document = agent::read_private_json(&path).unwrap();
    document["owner_extension"] = json!({"number":0.10960662360346242,"literal":"preserve"});
    let raw = format!(
        "  {}\n \n",
        serde_json::to_string_pretty(&document).unwrap()
    );
    fs::write(&path, &raw).unwrap();
    fs::write(
        directory.join("output.json"),
        b"previous output, including malformed JSON\0",
    )
    .unwrap();
    fs::set_permissions(
        directory.join("output.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    fs::write(directory.join("unknown.bin"), [0, 1, 255]).unwrap();
    fs::set_permissions(
        directory.join("unknown.bin"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let queue = directory.join("queue");
    agent::enqueue(&queue, "never replay the saved task", Some("saved-task")).unwrap();
    agent::atomic_json(
        &queue.join("inflight/ambiguous.json"),
        &json!({"text":"ambiguous","possibly_submitted":true}),
    )
    .unwrap();
    agent::atomic_json(
        &queue.join("failed/quarantined.json"),
        &json!({"text":"quarantined"}),
    )
    .unwrap();
    agent::atomic_json(
        &queue.join("failed/quarantined.json.error"),
        &json!({"outcome":"possibly_submitted"}),
    )
    .unwrap();
    assert!(
        !queue.join(".binding.lock").exists(),
        "enqueue leaves a valid lazy binding lock"
    );
    assert!(!queue.join("target.json").exists());
    let before = artifacts(&directory);
    let assertions = options(&fixture, &record);
    let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
    let result = offline_manager(&fixture, &client)
        .stop_with_options("foreign", assertions.clone())
        .unwrap();
    assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0);
    assert_eq!(client.liveness_calls.load(Ordering::Relaxed), 2);
    assert_eq!(result.as_object().unwrap().len(), 7);
    assert_eq!(result["name"], "foreign");
    assert_eq!(result["pane_closed"], false);
    assert_eq!(result["tab_closed"], false);
    assert_eq!(result["runtime_preserved"], true);
    assert_eq!(result["retired_dead_adoption"], true);
    assert_eq!(
        result["record_sha256"],
        assertions.expected_record_sha256.unwrap()
    );
    let archive = PathBuf::from(result["archive"].as_str().unwrap());
    assert_eq!(
        archive.file_name().unwrap(),
        format!("foreign-{}", record.token).as_str()
    );
    let after = artifacts(&archive);
    for (path, artifact) in &before {
        assert_eq!(after.get(path), Some(artifact), "{path:?}");
    }
    let additions: Vec<_> = after
        .keys()
        .filter(|path| !before.contains_key(*path))
        .collect();
    assert_eq!(additions, [&PathBuf::from("queue/.binding.lock")]);
    assert_eq!(
        fs::read(archive.join("agent.json")).unwrap(),
        raw.as_bytes()
    );
    assert_eq!(
        agent::read_private_json(&archive.join("agent.json")).unwrap()["lifecycle"],
        "running"
    );
    assert!(!archive.join("queue/target.json").exists());
    assert!(!directory.exists());
    assert!(fixture.client.closed.lock().unwrap().is_empty());
}

#[test]
fn dead_adoption_retirement_keeps_a_missing_queue_absent() {
    let (fixture, record) = adopted();
    let directory = fixture.root.join("registry/foreign");
    let before = artifacts(&directory);
    let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
    let result = offline_manager(&fixture, &client)
        .stop_with_options("foreign", options(&fixture, &record))
        .unwrap();
    let archive = PathBuf::from(result["archive"].as_str().unwrap());
    assert_eq!(artifacts(&archive), before);
    assert!(!archive.join("queue").exists());
    assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn dead_adoption_retirement_refuses_live_unknown_and_changed_final_death_proofs() {
    for states in [
        vec![ProcessLiveness::Alive],
        vec![ProcessLiveness::Unknown],
        vec![ProcessLiveness::Dead, ProcessLiveness::Alive],
        vec![ProcessLiveness::Dead, ProcessLiveness::Unknown],
    ] {
        let (fixture, record) = adopted();
        let path = fixture.root.join("registry/foreign/agent.json");
        let raw = fs::read(&path).unwrap();
        let client = OfflineClient::new(record.harness_anchor().unwrap(), &states);
        let failure = offline_manager(&fixture, &client)
            .advised_stop_with_options("foreign", options(&fixture, &record))
            .unwrap_err();
        assert_eq!(failure.error.exit_code(), 75, "{states:?}");
        assert_eq!(failure.recovery, RecoveryAction::Doctor);
        assert_eq!(fs::read(&path).unwrap(), raw);
        assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0);
        assert!(!fixture
            .root
            .join(format!("registry/archive/foreign-{}", record.token))
            .exists());
    }
}

#[test]
fn dead_adoption_retirement_requires_the_exact_token_raw_digest_and_selector() {
    for case in [
        "missing-token",
        "missing-hash",
        "stale-token",
        "empty-token",
        "nul-token",
        "stale-hash",
        "empty-hash",
        "uppercase-hash",
        "short-hash",
        "nul-hash",
        "both-selectors",
        "cloud-halt",
    ] {
        let (fixture, record) = adopted();
        let path = fixture.root.join("registry/foreign/agent.json");
        let raw = fs::read(&path).unwrap();
        let mut assertions = options(&fixture, &record);
        match case {
            "missing-token" => assertions.expected_token = None,
            "missing-hash" => assertions.expected_record_sha256 = None,
            "stale-token" => assertions.expected_token = Some("replacement-generation".to_owned()),
            "empty-token" => assertions.expected_token = Some(String::new()),
            "nul-token" => assertions.expected_token = Some("bad\0token".to_owned()),
            "stale-hash" => assertions.expected_record_sha256 = Some("0".repeat(64)),
            "empty-hash" => assertions.expected_record_sha256 = Some(String::new()),
            "uppercase-hash" => assertions.expected_record_sha256 = Some("A".repeat(64)),
            "short-hash" => assertions.expected_record_sha256 = Some("a".repeat(63)),
            "nul-hash" => assertions.expected_record_sha256 = Some(format!("{}\0", "a".repeat(63))),
            "both-selectors" => assertions.recover_legacy_adoption = true,
            "cloud-halt" => assertions.skip_cloud_halt = true,
            _ => unreachable!(),
        }
        let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
        let failure = offline_manager(&fixture, &client)
            .advised_stop_with_options("foreign", assertions)
            .unwrap_err();
        assert_eq!(failure.error.exit_code(), 75, "{case}");
        assert_eq!(failure.recovery, RecoveryAction::Doctor, "{case}");
        assert_eq!(fs::read(&path).unwrap(), raw, "{case}");
        assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0, "{case}");
    }
}

#[test]
fn dead_adoption_retirement_never_backfills_a_missing_or_obsolete_pin() {
    for case in [
        "missing-pin",
        "missing-rule",
        "old-rule",
        "invalid-pin",
        "managed",
        "headless",
        "other-backend",
        "stopped",
    ] {
        let (fixture, record) = adopted();
        let path = fixture.root.join("registry/foreign/agent.json");
        let mut document = agent::read_private_json(&path).unwrap();
        match case {
            "missing-pin" => document["harness_identity"] = Value::Null,
            "missing-rule" => document["anchor_rule"] = Value::Null,
            "old-rule" => document["anchor_rule"] = json!(ANCHOR_RULE - 1),
            "invalid-pin" => document["harness_identity"]["pid"] = json!(0),
            "managed" => {
                document["adapter"] = json!("herdr");
                document
                    .as_object_mut()
                    .unwrap()
                    .remove("foreign_shell_identity");
            }
            "headless" => document["mode"] = json!("headless"),
            "other-backend" => document["backend"] = json!("external"),
            "stopped" => document["lifecycle"] = json!("stopped"),
            _ => unreachable!(),
        }
        agent::atomic_json(&path, &document).unwrap();
        let raw = fs::read(&path).unwrap();
        let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
        let failure = offline_manager(&fixture, &client)
            .advised_stop_with_options("foreign", options(&fixture, &record))
            .unwrap_err();
        assert_eq!(failure.recovery, RecoveryAction::Doctor, "{case}");
        assert_eq!(client.liveness_calls.load(Ordering::Relaxed), 0, "{case}");
        assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0, "{case}");
        assert_eq!(fs::read(&path).unwrap(), raw, "{case}");
    }
}

#[test]
fn dead_adoption_retirement_refuses_a_valid_cloud_record_before_any_control_call() {
    let (fixture, mut record) = adopted();
    let old_pin = record.harness_anchor().unwrap().clone();
    let tool = fixture.root.join("cloud-control");
    fs::write(
        &tool,
        "#!/usr/bin/python3\nfrom pathlib import Path\nPath(__file__).with_name('cloud-called').write_text('called')\nraise SystemExit(91)\n",
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
    let session = "11111111-2222-3333-4444-555555555555";
    record.adapter = "agentcloud".to_owned();
    record.harness = "agentcloud".to_owned();
    record.foreign_shell_identity = None;
    record.harness_identity = None;
    record.anchor_rule = None;
    record.session_agent = Some("agentcloud".to_owned());
    record.session_value = Some(session.to_owned());
    record.native_session = Some(NativeSession::new("agentcloud", session, "observed"));
    record.agentcloud = Some(cloud::CloudRecord {
        launch: cloud::CloudLaunch::default(),
        agentterm: tool.display().to_string(),
        endpoint: Some("wss://example.invalid/ws".to_owned()),
        create_exit: Some(0),
        verified: Some(true),
        create_note: None,
        terminal_identity: None,
        halted: false,
        attached_self: false,
    });
    fixture.manager().save(&record).unwrap();
    let loaded = fixture.manager().load("foreign").unwrap();
    assert!(loaded.is_cloud());
    assert_eq!(
        loaded, record,
        "the test must exercise a valid parsed cloud record"
    );
    let path = fixture.root.join("registry/foreign/agent.json");
    let raw = fs::read(&path).unwrap();
    let client = OfflineClient::new(&old_pin, &[ProcessLiveness::Dead]);
    let manager = offline_manager(&fixture, &client).with_cloud_tools(CloudTools {
        agentcloudctl: tool.clone(),
        agentterm: tool,
        caller_session: None,
        endpoint: Some("wss://example.invalid/ws".to_owned()),
        endpoint_explicit: true,
        ambient_session: None,
        hostname: None,
    });
    for skip_cloud_halt in [false, true] {
        let mut assertions = options(&fixture, &record);
        assertions.skip_cloud_halt = skip_cloud_halt;
        let failure = manager
            .advised_stop_with_options("foreign", assertions)
            .unwrap_err();
        assert_eq!(failure.error.exit_code(), 75);
        assert_eq!(failure.recovery, RecoveryAction::Doctor);
        assert!(failure
            .to_string()
            .contains("only to a running interactive herdr-foreign record"));
        assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0);
        assert_eq!(client.liveness_calls.load(Ordering::Relaxed), 0);
        assert!(!fixture.root.join("cloud-called").exists());
        assert_eq!(fs::read(&path).unwrap(), raw);
        assert!(!fixture.root.join("registry/archive").exists());
    }
}

#[test]
fn dead_adoption_advice_is_captured_before_the_first_failing_runtime_probe() {
    let (fixture, record) = adopted();
    let assertions = options(&fixture, &record);
    let path = fixture.root.join("registry/foreign/agent.json");
    let original = fs::read(&path).unwrap();
    let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
    *client.probe_hook.lock().unwrap() = Some(Box::new({
        let path = path.clone();
        move || {
            let mut document = agent::read_private_json(&path).unwrap();
            document["token"] = json!("replacement-generation");
            agent::atomic_json(&path, &document).unwrap();
        }
    }));
    let failure = offline_manager(&fixture, &client)
        .advised_stop_with_options("foreign", StopOptions::default())
        .unwrap_err();
    assert_eq!(failure.error.exit_code(), 69);
    assert_eq!(failure.to_string(), "terminal server is unavailable");
    assert_eq!(
        failure.recovery,
        RecoveryAction::RetireDeadAdoption {
            name: "foreign".to_owned(),
            token: record.token.clone(),
            record_sha256: assertions.expected_record_sha256.clone().unwrap(),
        }
    );
    assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 1);
    let replacement = fs::read(&path).unwrap();
    let refused = offline_manager(&fixture, &client)
        .advised_stop_with_options("foreign", assertions.clone())
        .unwrap_err();
    assert_eq!(refused.recovery, RecoveryAction::Doctor);
    assert_eq!(fs::read(&path).unwrap(), replacement);
    assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 1);
    fs::write(&path, &original).unwrap();
    offline_manager(&fixture, &client)
        .stop_with_options("foreign", assertions)
        .unwrap();
    assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn dead_adoption_advice_does_not_replace_a_runtime_error_when_capture_is_unproved() {
    for case in [
        "live",
        "unknown",
        "record-hardlink",
        "queue-symlink",
        "lock-public",
    ] {
        let (fixture, record) = adopted();
        let directory = fixture.root.join("registry/foreign");
        let path = directory.join("agent.json");
        match case {
            "record-hardlink" => fs::hard_link(&path, fixture.root.join("record-link")).unwrap(),
            "queue-symlink" => {
                std::os::unix::fs::symlink(&fixture.root, directory.join("queue")).unwrap()
            }
            "lock-public" => {
                let queue = directory.join("queue");
                agent::enqueue(&queue, "retained", Some("retained")).unwrap();
                fs::set_permissions(
                    queue.join(".delivery.lock"),
                    fs::Permissions::from_mode(0o644),
                )
                .unwrap();
            }
            _ => {}
        }
        let state = match case {
            "live" => ProcessLiveness::Alive,
            "unknown" => ProcessLiveness::Unknown,
            _ => ProcessLiveness::Dead,
        };
        let client = OfflineClient::new(record.harness_anchor().unwrap(), &[state]);
        let original = fs::read(&path).unwrap();
        let failure = offline_manager(&fixture, &client)
            .advised_stop_with_options("foreign", StopOptions::default())
            .unwrap_err();
        assert_eq!(failure.error.exit_code(), 69, "{case}");
        assert_eq!(
            failure.to_string(),
            "terminal server is unavailable",
            "{case}"
        );
        assert_eq!(failure.recovery, RecoveryAction::Doctor, "{case}");
        assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 1, "{case}");
        assert_eq!(fs::read(&path).unwrap(), original, "{case}");
    }
}

#[test]
fn dead_adoption_retirement_rechecks_raw_file_directory_queue_and_lock_generations() {
    for case in [
        "raw-whitespace",
        "file-replacement",
        "directory-replacement",
        "new-queue",
        "queue-replacement",
        "lock-replacement",
    ] {
        let (fixture, record) = adopted();
        let directory = fixture.root.join("registry/foreign");
        let path = directory.join("agent.json");
        let original = fs::read(&path).unwrap();
        if matches!(case, "queue-replacement" | "lock-replacement") {
            agent::enqueue(&directory.join("queue"), "retained", Some("retained")).unwrap();
        }
        let assertions = options(&fixture, &record);
        let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
        let manager = offline_manager(&fixture, &client);
        let _name = manager.lock("foreign").unwrap();
        let _identity = manager.identity_lock().unwrap();
        let pinned = manager.pinned_agent_directory("foreign").unwrap();
        let queue = manager.retirement_queue_locks(&pinned).unwrap();
        let error =
            manager
                .retire_dead_adoption_locked_with(&record, &pinned, &queue, &assertions, || {
                    match case {
                        "raw-whitespace" => {
                            fs::write(&path, [original.as_slice(), b" \n"].concat()).unwrap()
                        }
                        "file-replacement" => {
                            fs::rename(&path, directory.join("original-record")).unwrap();
                            fs::write(&path, &original).unwrap();
                            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                        }
                        "directory-replacement" => {
                            fs::rename(&directory, fixture.root.join("original-directory"))
                                .unwrap();
                            agent::create_private_directory(
                                &directory,
                                "replacement test directory",
                                false,
                                false,
                            )
                            .unwrap();
                            fs::write(&path, &original).unwrap();
                            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                        }
                        "new-queue" => {
                            agent::enqueue(&directory.join("queue"), "new queue", Some("new"))
                                .map(|_| ())
                                .unwrap()
                        }
                        "queue-replacement" => {
                            fs::rename(directory.join("queue"), directory.join("original-queue"))
                                .unwrap();
                            agent::enqueue(
                                &directory.join("queue"),
                                "replacement",
                                Some("replacement"),
                            )
                            .unwrap();
                        }
                        "lock-replacement" => {
                            let queue = directory.join("queue");
                            fs::rename(
                                queue.join(".delivery.lock"),
                                queue.join("original-delivery-lock"),
                            )
                            .unwrap();
                            agent::open_private_lock(
                                &queue.join(".delivery.lock"),
                                "replacement test lock",
                            )
                            .unwrap();
                        }
                        _ => unreachable!(),
                    }
                })
                .unwrap_err();
        assert_eq!(error.exit_code(), 75, "{case}");
        assert!(directory.is_dir(), "{case}");
        assert!(
            !fixture
                .root
                .join(format!("registry/archive/foreign-{}", record.token))
                .exists(),
            "{case}"
        );
        assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0, "{case}");
    }
}

#[test]
fn dead_adoption_retirement_rechecks_mutations_during_the_final_kernel_proof() {
    for case in ["record", "queue", "lock"] {
        let (fixture, record) = adopted();
        let directory = fixture.root.join("registry/foreign");
        let path = directory.join("agent.json");
        let raw = fs::read(&path).unwrap();
        agent::enqueue(&directory.join("queue"), "retained", Some("retained")).unwrap();
        let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
        *client.proof_hook.lock().unwrap() = Some((
            2,
            Box::new({
                let directory = directory.clone();
                let path = path.clone();
                let raw = raw.clone();
                move || match case {
                    "record" => fs::write(path, [raw.as_slice(), b" \n"].concat()).unwrap(),
                    "queue" => {
                        fs::rename(directory.join("queue"), directory.join("displaced-queue"))
                            .unwrap()
                    }
                    "lock" => {
                        let queue = directory.join("queue");
                        fs::rename(
                            queue.join(".binding.lock"),
                            queue.join("displaced-binding-lock"),
                        )
                        .unwrap();
                        agent::open_private_lock(
                            &queue.join(".binding.lock"),
                            "replacement test lock",
                        )
                        .unwrap();
                    }
                    _ => unreachable!(),
                }
            }),
        ));
        let failure = offline_manager(&fixture, &client)
            .advised_stop_with_options("foreign", options(&fixture, &record))
            .unwrap_err();
        assert_eq!(failure.recovery, RecoveryAction::Doctor, "{case}");
        assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0, "{case}");
        assert!(directory.is_dir(), "{case}");
        assert!(
            !fixture
                .root
                .join(format!("registry/archive/foreign-{}", record.token))
                .exists(),
            "{case}"
        );
    }
}

#[test]
fn dead_adoption_retirement_refuses_an_existing_archive_and_preserves_it() {
    let (fixture, record) = adopted();
    let archive = fixture
        .root
        .join(format!("registry/archive/foreign-{}", record.token));
    agent::create_private_directory(&archive, "test archive", true, false).unwrap();
    fs::write(archive.join("retained"), b"never overwrite").unwrap();
    fs::set_permissions(archive.join("retained"), fs::Permissions::from_mode(0o600)).unwrap();
    let before = artifacts(&archive);
    let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
    offline_manager(&fixture, &client)
        .stop_with_options("foreign", options(&fixture, &record))
        .unwrap_err();
    assert_eq!(artifacts(&archive), before);
    assert!(fixture.root.join("registry/foreign/agent.json").is_file());
    assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn dead_adoption_retirement_preserves_a_late_archive_collision() {
    let (fixture, record) = adopted();
    let archive = fixture
        .root
        .join(format!("registry/archive/foreign-{}", record.token));
    let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
    *client.proof_hook.lock().unwrap() = Some((
        2,
        Box::new({
            let archive = archive.clone();
            move || {
                agent::create_private_directory(&archive, "late test archive", false, false)
                    .unwrap();
                fs::write(
                    archive.join("retained"),
                    b"late collision must remain untouched",
                )
                .unwrap();
                fs::set_permissions(archive.join("retained"), fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
        }),
    ));
    let directory = fixture.root.join("registry/foreign");
    let before = artifacts(&directory);
    let failure = offline_manager(&fixture, &client)
        .advised_stop_with_options("foreign", options(&fixture, &record))
        .unwrap_err();
    assert_eq!(failure.recovery, RecoveryAction::Doctor);
    assert_eq!(artifacts(&directory), before);
    assert_eq!(
        fs::read(archive.join("retained")).unwrap(),
        b"late collision must remain untouched"
    );
    assert_eq!(fs::read_dir(&archive).unwrap().count(), 1);
    assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn dead_adoption_retirement_holds_all_authority_locks_before_the_kernel_proof() {
    let (fixture, record) = adopted();
    agent::enqueue(
        &fixture.root.join("registry/foreign/queue"),
        "retained",
        Some("retained"),
    )
    .unwrap();
    let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
    *client.proof_hook.lock().unwrap() = Some((
        1,
        Box::new({
            let registry = fixture.root.join("registry");
            move || {
                for suffix in [
                    ".foreign.lock",
                    ".identity.lock",
                    "foreign/queue/.delivery.lock",
                    "foreign/queue/.binding.lock",
                ] {
                    let file = agent::open_private_lock(
                        &registry.join(suffix),
                        "test held retirement lock",
                    )
                    .unwrap();
                    assert!(
                        FileExt::try_lock_exclusive(&file).is_err(),
                        "{suffix} must already be held"
                    );
                }
            }
        }),
    ));
    offline_manager(&fixture, &client)
        .stop_with_options("foreign", options(&fixture, &record))
        .unwrap();
    assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn dead_adoption_advice_rejects_changes_during_its_final_death_proof() {
    let (fixture, record) = adopted();
    let path = fixture.root.join("registry/foreign/agent.json");
    let original = fs::read(&path).unwrap();
    let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
    *client.proof_hook.lock().unwrap() = Some((
        2,
        Box::new({
            let path = path.clone();
            let original = original.clone();
            move || fs::write(path, [original.as_slice(), b" \n"].concat()).unwrap()
        }),
    ));
    let failure = offline_manager(&fixture, &client)
        .advised_stop_with_options("foreign", StopOptions::default())
        .unwrap_err();
    assert_eq!(failure.error.exit_code(), 69);
    assert_eq!(failure.to_string(), "terminal server is unavailable");
    assert_eq!(failure.recovery, RecoveryAction::Doctor);
    assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        fs::read(&path).unwrap(),
        [original.as_slice(), b" \n"].concat()
    );
}

#[test]
fn dead_adoption_retirement_refuses_unsafe_private_record_and_queue_authority() {
    for case in [
        "record-hardlink",
        "record-public",
        "record-symlink",
        "record-oversize",
        "directory-public",
        "queue-symlink",
        "queue-public",
        "lock-symlink",
        "lock-hardlink",
        "lock-public",
        "lock-fifo",
    ] {
        let (fixture, record) = adopted();
        let directory = fixture.root.join("registry/foreign");
        let path = directory.join("agent.json");
        let assertions = options(&fixture, &record);
        let raw = fs::read(&path).unwrap();
        if case.starts_with("lock-") || case == "queue-public" {
            agent::enqueue(&directory.join("queue"), "retained", Some("retained")).unwrap();
        }
        let lock = directory.join("queue/.delivery.lock");
        match case {
            "record-hardlink" => fs::hard_link(&path, fixture.root.join("record-link")).unwrap(),
            "record-public" => {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap()
            }
            "record-symlink" => {
                fs::rename(&path, fixture.root.join("original-record")).unwrap();
                std::os::unix::fs::symlink(fixture.root.join("original-record"), &path).unwrap();
            }
            "record-oversize" => fs::write(
                &path,
                [
                    raw.as_slice(),
                    " ".repeat(MAX_AGENT_RECORD_BYTES).as_bytes(),
                ]
                .concat(),
            )
            .unwrap(),
            "directory-public" => {
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap()
            }
            "queue-symlink" => {
                std::os::unix::fs::symlink(&fixture.root, directory.join("queue")).unwrap()
            }
            "queue-public" => {
                fs::set_permissions(directory.join("queue"), fs::Permissions::from_mode(0o755))
                    .unwrap()
            }
            "lock-symlink" => {
                fs::rename(&lock, directory.join("original-lock")).unwrap();
                std::os::unix::fs::symlink(directory.join("original-lock"), &lock).unwrap();
            }
            "lock-hardlink" => fs::hard_link(&lock, directory.join("lock-link")).unwrap(),
            "lock-public" => fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap(),
            "lock-fifo" => {
                fs::rename(&lock, directory.join("original-lock")).unwrap();
                let name = CString::new(lock.as_os_str().as_encoded_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
        let failure = offline_manager(&fixture, &client)
            .advised_stop_with_options("foreign", assertions)
            .unwrap_err();
        assert_eq!(failure.recovery, RecoveryAction::Doctor, "{case}");
        assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0, "{case}");
        assert!(directory.is_dir(), "{case}");
        assert!(!fixture.root.join("registry/archive").exists(), "{case}");
    }
}

#[test]
fn dead_adoption_retirement_respects_pending_name_and_move_reservations() {
    for operation in ["rename", "move", "corrupt-revive"] {
        let (fixture, record) = adopted();
        let manager = fixture.manager();
        match operation {
            "rename" => manager
                .write_rename_journal(&RenameJournal {
                    token: record.token.clone(),
                    old: record.name.clone(),
                    new: "reviewer".to_owned(),
                    adapter: record.adapter.clone(),
                    pane_id: record.pane_id.clone().unwrap(),
                    tab_id: record.tab_id.clone().unwrap(),
                    terminal_id: record.terminal_id.clone(),
                    workspace_id: record.workspace_id.clone(),
                    journal_id: new_journal_id().unwrap(),
                    started_at: unix_seconds(),
                })
                .unwrap(),
            "move" => manager
                .write_move_intent(&record, "project-workspace")
                .unwrap(),
            "corrupt-revive" => {
                let directory = fixture.root.join("registry/.revives");
                agent::create_private_directory(&directory, "test pending revive", false, false)
                    .unwrap();
                agent::atomic_json(
                    &directory.join(format!("{}.json", record.token)),
                    &json!({}),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let client = OfflineClient::new(record.harness_anchor().unwrap(), &[ProcessLiveness::Dead]);
        let failure = offline_manager(&fixture, &client)
            .advised_stop_with_options("foreign", options(&fixture, &record))
            .unwrap_err();
        assert_eq!(failure.error.exit_code(), 75, "{operation}");
        assert_eq!(
            client.liveness_calls.load(Ordering::Relaxed),
            0,
            "{operation}"
        );
        assert_eq!(client.rpc_calls.load(Ordering::Relaxed), 0, "{operation}");
        assert!(
            fixture.root.join("registry/foreign/agent.json").is_file(),
            "{operation}"
        );
    }
}

#[test]
fn dead_adoption_retirement_does_not_change_ordinary_live_foreign_stop() {
    let (fixture, record) = adopted();
    let result = fixture.manager().stop("foreign").unwrap();
    assert_eq!(result["runtime_preserved"], true);
    assert!(result.get("retired_dead_adoption").is_none());
    let archive = PathBuf::from(result["archive"].as_str().unwrap());
    assert_eq!(
        agent::read_private_json(&archive.join("agent.json")).unwrap()["lifecycle"],
        "stopped"
    );
    assert!(archive.join("output.json").is_file());
    assert!(fixture.client.started.load(Ordering::Relaxed));
    assert_eq!(
        fixture.client.harness_pids.lock().unwrap()["owned"],
        record.harness_anchor().unwrap().pid
    );
    assert!(fixture.client.closed.lock().unwrap().is_empty());
}

#[test]
fn dead_adoption_supplemental_capture_failure_keeps_ordinary_foreign_stop_success() {
    let (fixture, _) = adopted();
    let queue = fixture.root.join("registry/foreign/queue");
    agent::enqueue(&queue, "retained", Some("retained")).unwrap();
    fs::set_permissions(
        queue.join(".delivery.lock"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let payload = fs::read(queue.join("inbox/retained.json")).unwrap();
    let result = fixture.manager().stop("foreign").unwrap();
    assert_eq!(result["runtime_preserved"], true);
    assert!(result.get("retired_dead_adoption").is_none());
    let archive = PathBuf::from(result["archive"].as_str().unwrap());
    assert_eq!(
        fs::read(archive.join("queue/inbox/retained.json")).unwrap(),
        payload
    );
    assert!(fixture.client.started.load(Ordering::Relaxed));
    assert!(fixture.client.closed.lock().unwrap().is_empty());
}

#[cfg(target_os = "linux")]
struct ChildGuard(std::process::Child);

#[cfg(target_os = "linux")]
impl ChildGuard {
    fn start() -> Self {
        let child = Self(
            std::process::Command::new("/usr/bin/sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        );
        let executable = fs::canonicalize("/usr/bin/sleep").unwrap();
        for _ in 0..200 {
            if fs::read_link(format!("/proc/{}/exe", child.0.id()))
                .ok()
                .as_ref()
                == Some(&executable)
            {
                return child;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("fixture child never executed sleep");
    }
    fn identity(&self) -> CustomProcessIdentity {
        let pid = self.0.id();
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let executable = fs::metadata(format!("/proc/{pid}/exe")).unwrap();
        CustomProcessIdentity {
            version: 1,
            boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .unwrap()
                .trim()
                .to_owned(),
            pid: u64::from(pid),
            starttime_ticks: fields[19].parse().unwrap(),
            executable_device: executable.dev(),
            executable_inode: executable.ino(),
        }
    }
    fn kill(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

#[cfg(target_os = "linux")]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn dead_adoption_retirement_uses_kernel_death_with_the_control_executable_absent() {
    let (fixture, mut record) = adopted();
    let mut process = ChildGuard::start();
    let identity = process.identity();
    record.harness_identity = Some(identity.clone());
    fixture.manager().save(&record).unwrap();
    let client =
        HerdrClient::with_executable("direct", &fixture.root.join("absent-herdr")).unwrap();
    let manager = ManagedAgents::new(&client, &fixture.root.join("registry")).unwrap();
    let path = fixture.root.join("registry/foreign/agent.json");
    let raw = fs::read(&path).unwrap();
    let error = manager
        .stop_with_options("foreign", options(&fixture, &record))
        .unwrap_err();
    assert!(error.to_string().contains("still alive"));
    assert_eq!(process.identity(), identity);
    assert_eq!(fs::read(&path).unwrap(), raw);
    record.harness_identity.as_mut().unwrap().executable_inode += 1;
    fixture.manager().save(&record).unwrap();
    let error = manager
        .stop_with_options("foreign", options(&fixture, &record))
        .unwrap_err();
    assert!(
        error.to_string().contains("cannot prove"),
        "same generation with changed image stays unknown"
    );
    assert_eq!(process.identity(), identity);
    record.harness_identity = Some(identity);
    fixture.manager().save(&record).unwrap();
    let raw = fs::read(&path).unwrap();
    process.kill();
    let result = manager
        .stop_with_options("foreign", options(&fixture, &record))
        .unwrap();
    assert_eq!(
        fs::read(PathBuf::from(result["archive"].as_str().unwrap()).join("agent.json")).unwrap(),
        raw
    );
    assert!(!path.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn dead_adoption_retirement_preserves_a_live_reused_pid_and_changed_boot() {
    for changed_boot in [false, true] {
        let (fixture, mut record) = adopted();
        let process = ChildGuard::start();
        let replacement = process.identity();
        let mut old = replacement.clone();
        if changed_boot {
            old.boot_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned();
            assert_ne!(old.boot_id, replacement.boot_id);
        } else {
            old.starttime_ticks -= 1;
        }
        record.harness_identity = Some(old);
        fixture.manager().save(&record).unwrap();
        let client =
            HerdrClient::with_executable("direct", &fixture.root.join("absent-herdr")).unwrap();
        let manager = ManagedAgents::new(&client, &fixture.root.join("registry")).unwrap();
        let result = manager
            .stop_with_options("foreign", options(&fixture, &record))
            .unwrap();
        assert_eq!(result["retired_dead_adoption"], true);
        assert_eq!(
            process.identity(),
            replacement,
            "replacement process must remain untouched"
        );
        assert_eq!(unsafe { libc::kill(process.0.id() as libc::pid_t, 0) }, 0);
    }
}
