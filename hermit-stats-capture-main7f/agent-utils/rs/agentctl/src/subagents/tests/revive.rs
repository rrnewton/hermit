use super::*;

const TIMEOUT: Duration = Duration::from_secs(30);

fn kill_owned(fixture: &Fixture, record: &AgentRecord) {
    fixture
        .client
        .dead_panes
        .lock()
        .unwrap()
        .insert(record.pane_id.clone().unwrap());
    fixture.client.started.store(false, Ordering::Relaxed);
    fixture.client.custom_alive.store(false, Ordering::Relaxed);
}

fn recoverable(harness: &str, reports_session: bool) -> (Fixture, AgentRecord) {
    let fixture = Fixture::new();
    fixture
        .client
        .fresh_presentations
        .store(true, Ordering::Relaxed);
    fixture
        .client
        .report_session
        .store(reports_session, Ordering::Relaxed);
    fixture
        .manager()
        .start(
            "worker",
            &fixture.root,
            StartOptions {
                harness: harness.to_owned(),
                ..StartOptions::default()
            },
        )
        .unwrap();
    let record = fixture.manager().load("worker").unwrap();
    kill_owned(&fixture, &record);
    (fixture, record)
}

fn journal_path(fixture: &Fixture, record: &AgentRecord) -> PathBuf {
    fixture
        .root
        .join("registry/.revives")
        .join(format!("{}.json", record.token))
}

fn stage_path(fixture: &Fixture, record: &AgentRecord) -> PathBuf {
    fixture
        .root
        .join("registry/.revives")
        .join(&record.token)
        .join("new")
}

type FileSnapshot = BTreeMap<PathBuf, (u64, u64, u32, Option<Vec<u8>>)>;

fn files(path: &Path) -> FileSnapshot {
    fn visit(root: &Path, path: &Path, result: &mut FileSnapshot) {
        let metadata = fs::symlink_metadata(path).unwrap();
        result.insert(
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
                visit(root, &entry.unwrap().path(), result);
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(path, path, &mut result);
    result
}

#[test]
fn revive_resumes_without_reports_using_independent_process_and_terminal_anchors() {
    let (fixture, old) = recoverable("claude", false);
    let native = old.native_session.clone().unwrap();
    let result = fixture
        .manager()
        .revive("worker", false, Some(&old.token), TIMEOUT)
        .unwrap();
    assert_eq!(result["revived"], true);
    assert_eq!(result["previous_token"], old.token);
    assert_eq!(result["resume"], native.value);
    assert_eq!(result["session_value"], Value::Null);
    assert_ne!(result["token"], old.token);
    assert_eq!(result["pane_closed"], true);
    assert_eq!(result["tab_closed"], true);
    let new = fixture.manager().load("worker").unwrap();
    assert!(new.harness_anchor().is_some());
    assert!(new.terminal_id.is_some());
    assert_eq!(new.arguments, ["--resume", &native.value]);
    assert_eq!(new.extra["revived_from"], old.token);
    assert!(new.storage_directory.is_none());
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
    assert_eq!(
        *fixture.client.closed.lock().unwrap(),
        [old.pane_id.clone().unwrap()]
    );
    assert!(!journal_path(&fixture, &old).exists());
}

#[test]
fn revive_preserves_entire_old_queue_and_requested_task_without_replaying_either() {
    let (fixture, mut old) = recoverable("codex", true);
    old.paused = true;
    old.goal = Some("Finish the original task".to_owned());
    old.goal_delivery = Some("delivered".to_owned());
    old.goal_session_id = old.session_value.clone();
    old.goal_message_id = Some("goal-1".to_owned());
    old.goal_messages
        .insert("goal-1".to_owned(), old.goal.clone().unwrap());
    old.extra
        .insert("owner_extension".to_owned(), json!({"retained":true}));
    fixture.manager().save(&old).unwrap();
    let old_directory = fixture.root.join("registry/worker");
    let queue = old_directory.join("queue");
    agent::create_private_directory(&queue, "test old queue", false, false).unwrap();
    for directory in ["pending", "failed", "processed"] {
        let path = queue.join(directory);
        agent::create_private_directory(&path, "test queue lane", false, false).unwrap();
        agent::atomic_json(
            &path.join("message.json"),
            &json!({"text":"Do not replay", "lane":directory}),
        )
        .unwrap();
    }
    for name in [".delivery.lock", ".binding.lock"] {
        agent::open_private_lock(&queue.join(name), "test queue lock").unwrap();
    }
    let before = files(&queue);
    let old_bytes = fs::read(old_directory.join("agent.json")).unwrap();
    let result = fixture
        .manager()
        .revive("worker", false, Some(&old.token), TIMEOUT)
        .unwrap();
    let archive = PathBuf::from(result["archive"].as_str().unwrap());
    assert_eq!(files(&archive.join("queue")), before);
    let mut expected: Value = serde_json::from_slice(&old_bytes).unwrap();
    expected["lifecycle"] = json!("stopped");
    assert_eq!(
        agent::read_private_json(&archive.join("agent.json")).unwrap(),
        expected
    );
    let new = fixture.manager().load("worker").unwrap();
    assert!(new.paused);
    assert_eq!(new.goal, old.goal);
    assert_eq!(new.extra["owner_extension"], old.extra["owner_extension"]);
    assert!(
        new.goal_delivery.is_none()
            && new.goal_session_id.is_none()
            && new.goal_message_id.is_none()
    );
    assert!(new.goal_messages.is_empty());
    assert!(fixture.client.runs.lock().unwrap().is_empty());
    assert!(fixture.client.keys_sent.lock().unwrap().is_empty());
    assert!(!fixture
        .root
        .join("registry/worker/queue/pending/message.json")
        .exists());
}

#[test]
fn revive_preserves_producer_decimals_in_stopped_and_resumed_generations() {
    let timestamp = 1_791_594_983.664_875_7;
    let numbers = json!({
        "timestamp": timestamp,
        "nested": [0.123_456_789_012_345_68, {"signed": -timestamp}],
    });
    for point in ["ready", "published"] {
        let (fixture, mut old) = recoverable("codex", true);
        old.created_at = timestamp;
        old.extra
            .insert("preserved_numbers".to_owned(), numbers.clone());
        fixture.manager().save(&old).unwrap();
        let original: Value = serde_json::from_slice(
            &fs::read(fixture.root.join("registry/worker/agent.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(original["created_at"].as_f64(), Some(timestamp));
        assert_eq!(original["preserved_numbers"], numbers);

        super::super::revive::fail_once(point);
        fixture
            .manager()
            .revive("worker", false, Some(&old.token), TIMEOUT)
            .unwrap_err();
        let operation = fixture.root.join("registry/.revives").join(&old.token);
        let stopped = agent::read_private_json(&operation.join("stopped.json")).unwrap();
        let mut expected_stopped = original;
        expected_stopped["lifecycle"] = json!("stopped");
        assert_eq!(stopped, expected_stopped);
        let candidate_path = if point == "published" {
            fixture.root.join("registry/worker/agent.json")
        } else {
            operation.join("new/agent.json")
        };
        let candidate = agent::read_private_json(&candidate_path).unwrap();
        assert_eq!(candidate["preserved_numbers"], numbers);
        assert_eq!(
            fixture
                .manager()
                .revive("worker", true, Some(&old.token), TIMEOUT)
                .unwrap()["action"],
            "recover"
        );
        let result = fixture
            .manager()
            .revive("worker", false, Some(&old.token), TIMEOUT)
            .unwrap();
        let archive = PathBuf::from(result["archive"].as_str().unwrap());
        assert_eq!(
            agent::read_private_json(&archive.join("agent.json")).unwrap(),
            expected_stopped
        );
        assert_eq!(
            fixture.manager().load("worker").unwrap().extra["preserved_numbers"],
            numbers
        );
        assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
    }
}

#[test]
fn revive_dry_run_is_an_immutable_safe_plan() {
    let (fixture, old) = recoverable("claude", false);
    let registry = fixture.root.join("registry");
    let before = files(&registry);
    let plan = fixture
        .manager()
        .revive("worker", true, Some(&old.token), TIMEOUT)
        .unwrap();
    assert_eq!(plan["action"], "revive");
    assert_eq!(plan["native_session"], json!(old.native_session));
    assert!(plan.get("arguments").is_none() && plan.get("environment").is_none());
    assert_eq!(files(&registry), before);
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 1);
    assert!(fixture.client.closed.lock().unwrap().is_empty());
}

#[test]
fn revive_live_unknown_and_unanchored_processes_never_allocate_a_tab() {
    for (state, action) in [
        ("live", "skip"),
        ("unknown", "blocked"),
        ("unanchored", "blocked"),
    ] {
        let (fixture, mut old) = recoverable("claude", false);
        match state {
            "live" => {
                fixture.client.dead_panes.lock().unwrap().clear();
                fixture.client.started.store(true, Ordering::Relaxed);
            }
            "unknown" => fixture
                .client
                .liveness_unknown
                .store(true, Ordering::Relaxed),
            _ => {
                old.harness_identity = None;
                old.anchor_rule = None;
                fixture.manager().save(&old).unwrap();
            }
        }
        let before = files(&fixture.root.join("registry"));
        assert_eq!(
            fixture
                .manager()
                .revive("worker", true, None, TIMEOUT)
                .unwrap()["action"],
            action
        );
        assert!(fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .is_err());
        // Actual attempts may create lifecycle locks; all record bytes are retained.
        assert_eq!(
            fs::read(fixture.root.join("registry/worker/agent.json")).unwrap(),
            before[Path::new("worker/agent.json")].3.clone().unwrap()
        );
        assert_eq!(fixture.client.environments.lock().unwrap().len(), 1);
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }
}

#[test]
fn revive_all_skips_live_or_nonrunning_records_while_named_actual_refuses_them() {
    for state in ["live", "stopped"] {
        let (fixture, mut old) = recoverable("claude", false);
        if state == "live" {
            fixture.client.dead_panes.lock().unwrap().clear();
            fixture.client.started.store(true, Ordering::Relaxed);
        } else {
            old.lifecycle = "stopped".to_owned();
            fixture.manager().save(&old).unwrap();
        }
        assert!(
            fixture
                .manager()
                .revive("worker", false, None, TIMEOUT)
                .is_err(),
            "{state}"
        );
        let result = fixture.manager().revive_all(false, TIMEOUT).unwrap();
        assert_eq!(result["revived"], 0);
        assert_eq!(result["blocked"], 0);
        assert_eq!(result["agents"][0]["action"], "skip", "{state}");
        assert_eq!(fixture.client.environments.lock().unwrap().len(), 1);
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }
}

#[test]
fn revive_refuses_unsupported_aliases_and_unsafe_environment_replay() {
    for kind in [
        "adopted",
        "headless",
        "environment",
        "missing-conversation",
        "move",
        "sibling",
    ] {
        let (fixture, mut old) = recoverable("codex", true);
        match kind {
            "adopted" => {
                old.adapter = "herdr-foreign".to_owned();
                old.foreign_shell_identity = Some(Fake::foreign_shell_identity());
            }
            "headless" => {
                old.mode = "headless".to_owned();
                old.backend = "worker".to_owned();
            }
            "environment" => {
                old.environment_names = vec!["PRIVATE_MODE".to_owned()];
            }
            "missing-conversation" => {
                old.native_session = None;
                old.session_agent = None;
                old.session_value = None;
                old.resume = None;
            }
            "move" => {
                fixture
                    .manager()
                    .write_move_intent(&old, "project-workspace")
                    .unwrap();
            }
            "sibling" => fixture
                .client
                .panes
                .lock()
                .unwrap()
                .push(Fake::pane("human")),
            _ => unreachable!(),
        }
        fixture.manager().save(&old).unwrap();
        assert_eq!(
            fixture
                .manager()
                .revive("worker", true, None, TIMEOUT)
                .unwrap()["action"],
            "blocked",
            "{kind}"
        );
        assert!(
            fixture
                .manager()
                .revive("worker", false, None, TIMEOUT)
                .is_err(),
            "{kind}"
        );
        assert_eq!(
            fixture.client.environments.lock().unwrap().len(),
            1,
            "{kind}"
        );
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }
}

#[test]
fn revive_requires_positive_missing_target_and_absent_unambiguous_census() {
    let (fixture, old) = recoverable("claude", false);
    let pane = old.pane_id.as_ref().unwrap();
    fixture
        .client
        .panes
        .lock()
        .unwrap()
        .retain(|item| &item.pane_id != pane);
    assert_eq!(
        fixture
            .manager()
            .revive("worker", true, None, TIMEOUT)
            .unwrap()["action"],
        "blocked"
    );
    fixture
        .client
        .herdr_closed
        .lock()
        .unwrap()
        .insert(pane.clone());
    let result = fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .unwrap();
    assert_eq!(result["revived"], true);
    assert!(fixture.client.closed.lock().unwrap().is_empty());
}

#[test]
fn revive_refuses_a_missing_old_pane_when_its_tab_has_been_reused() {
    let (fixture, old) = recoverable("claude", false);
    let pane = old.pane_id.as_ref().unwrap();
    let tab = old.tab_id.as_ref().unwrap();
    fixture
        .client
        .panes
        .lock()
        .unwrap()
        .retain(|item| &item.pane_id != pane);
    fixture
        .client
        .herdr_closed
        .lock()
        .unwrap()
        .insert(pane.clone());
    let mut unrelated = Fake::pane("human");
    unrelated.tab_id = tab.clone();
    fixture.client.panes.lock().unwrap().push(unrelated);
    let before = files(&fixture.root.join("registry"));
    assert_eq!(
        fixture
            .manager()
            .revive("worker", true, None, TIMEOUT)
            .unwrap()["action"],
        "blocked"
    );
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .is_err());
    assert!(!journal_path(&fixture, &old).exists());
    assert_eq!(files(&fixture.root.join("registry")), before);
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 1);
    assert!(fixture.client.closed.lock().unwrap().is_empty());
}

#[test]
fn revive_refuses_changed_pause_policy_before_candidate_ready() {
    let (fixture, mut old) = recoverable("claude", false);
    old.paused = true;
    fixture.manager().save(&old).unwrap();
    super::super::revive::fail_once("launched");
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .is_err());
    let path = stage_path(&fixture, &old).join("agent.json");
    let mut candidate = agent::read_private_json(&path).unwrap();
    candidate["paused"] = json!(false);
    agent::atomic_json(&path, &candidate).unwrap();
    let before = files(&fixture.root.join("registry"));
    assert_eq!(
        fixture
            .manager()
            .revive("worker", true, None, TIMEOUT)
            .unwrap()["action"],
        "blocked"
    );
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .is_err());
    assert_eq!(files(&fixture.root.join("registry")), before);
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
    assert!(fixture.client.closed.lock().unwrap().is_empty());
}

#[test]
fn revive_refuses_partial_or_different_provider_legacy_routing_identity() {
    for provider in [None, Some("claude")] {
        let (fixture, mut old) = recoverable("codex", true);
        old.native_session = None;
        old.session_agent = provider.map(str::to_owned);
        fixture.manager().save(&old).unwrap();
        let before = files(&fixture.root.join("registry"));
        let plan = fixture
            .manager()
            .revive("worker", true, None, TIMEOUT)
            .unwrap();
        assert_eq!(plan["action"], "blocked");
        assert!(plan["reason"].as_str().unwrap().contains("provider"));
        assert!(fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .is_err());
        assert_eq!(files(&fixture.root.join("registry")), before);
        assert_eq!(fixture.client.environments.lock().unwrap().len(), 1);
    }
}

#[test]
fn revive_pending_dry_run_detects_replaced_old_directory_and_unstopped_archive() {
    for modification in ["directory", "archive"] {
        let (fixture, old) = recoverable("claude", false);
        let original = fs::read(fixture.root.join("registry/worker/agent.json")).unwrap();
        super::super::revive::fail_once(if modification == "directory" {
            "ready"
        } else {
            "archived"
        });
        assert!(fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .is_err());
        if modification == "directory" {
            let active = fixture.root.join("registry/worker");
            fs::rename(&active, fixture.root.join("retained-original")).unwrap();
            DirBuilder::new().mode(0o700).create(&active).unwrap();
            fs::write(active.join("agent.json"), &original).unwrap();
            fs::set_permissions(active.join("agent.json"), fs::Permissions::from_mode(0o600))
                .unwrap();
        } else {
            let path = fixture
                .root
                .join("registry/archive")
                .join(format!("worker-{}", old.token))
                .join("agent.json");
            fs::write(path, &original).unwrap();
        }
        let before = files(&fixture.root.join("registry"));
        assert_eq!(
            fixture
                .manager()
                .revive("worker", true, None, TIMEOUT)
                .unwrap()["action"],
            "blocked",
            "{modification}"
        );
        assert!(
            fixture
                .manager()
                .revive("worker", false, None, TIMEOUT)
                .is_err(),
            "{modification}"
        );
        assert_eq!(
            files(&fixture.root.join("registry")),
            before,
            "{modification}"
        );
        assert_eq!(
            fixture.client.environments.lock().unwrap().len(),
            2,
            "{modification}"
        );
        assert!(
            fixture.client.closed.lock().unwrap().is_empty(),
            "{modification}"
        );
    }
}

#[test]
fn revive_pending_journal_refuses_symlink_or_hardlink_authority() {
    for link in ["symlink", "hardlink"] {
        let (fixture, old) = recoverable("claude", false);
        super::super::revive::fail_once("ready");
        assert!(fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .is_err());
        let path = journal_path(&fixture, &old);
        let saved = path.with_extension("retained");
        fs::rename(&path, &saved).unwrap();
        if link == "symlink" {
            std::os::unix::fs::symlink(&saved, &path).unwrap();
        } else {
            fs::hard_link(&saved, &path).unwrap();
        }
        let before = files(&fixture.root.join("registry"));
        assert!(
            fixture
                .manager()
                .revive("worker", true, None, TIMEOUT)
                .is_err(),
            "{link}"
        );
        assert!(
            fixture
                .manager()
                .revive("worker", false, None, TIMEOUT)
                .is_err(),
            "{link}"
        );
        assert_eq!(files(&fixture.root.join("registry")), before, "{link}");
        assert_eq!(
            fixture.client.environments.lock().unwrap().len(),
            2,
            "{link}"
        );
        assert!(fixture.client.closed.lock().unwrap().is_empty(), "{link}");
    }
}

#[test]
fn revive_muse_preserves_globals_before_the_resume_subcommand() {
    let (fixture, old) = recoverable("muse", true);
    let result = fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .unwrap();
    assert_eq!(result["revived"], true);
    let new = fixture.manager().load("worker").unwrap();
    assert_eq!(
        new.arguments,
        ["resume", &old.native_session.unwrap().value]
    );
    assert!(new.custom_process_identity.is_some());
    assert_eq!(
        harness_arguments(
            "muse",
            Some("model"),
            Some("native"),
            &[
                "--reasoning-effort".to_owned(),
                "high".to_owned(),
                "--permission-mode".to_owned(),
                "owner-policy".to_owned()
            ]
        )
        .unwrap(),
        [
            "--model",
            "model",
            "--reasoning-effort",
            "high",
            "--permission-mode",
            "owner-policy",
            "resume",
            "native"
        ]
    );
}

#[test]
fn revive_resumes_muse_with_a_proved_stale_native_report_and_reconciles_each_boundary() {
    for interruption in [None, Some("launched"), Some("ready"), Some("archived")] {
        let (fixture, old) = recoverable("muse", true);
        fixture
            .client
            .stale_reported_panes
            .lock()
            .unwrap()
            .insert(old.pane_id.clone().unwrap());
        let info = fixture
            .client
            .pane_info(old.pane_id.as_ref().unwrap())
            .unwrap();
        assert_eq!(info.agent.as_deref(), Some("muse"));
        assert_eq!(info.session_agent, old.session_agent);
        assert_eq!(info.session_value, old.session_value);
        assert!(old.pane_reported_by_agentctl);
        if let Some(point) = interruption {
            super::super::revive::fail_once(point);
            assert!(fixture
                .manager()
                .revive("worker", false, None, TIMEOUT)
                .is_err());
            let stage: AgentRecord = serde_json::from_value(
                agent::read_private_json(&stage_path(&fixture, &old).join("agent.json")).unwrap(),
            )
            .unwrap();
            let ordinary =
                agent::resolve_target(&fixture.client, &fixture.manager().target(&stage).unwrap())
                    .unwrap_err()
                    .to_string();
            assert!(ordinary.contains("found 2"), "ordinary census: {ordinary}");
            let before = files(&fixture.root.join("registry"));
            assert_eq!(
                fixture
                    .manager()
                    .revive("worker", true, None, TIMEOUT)
                    .unwrap()["action"],
                "recover",
                "{point}"
            );
            assert_eq!(files(&fixture.root.join("registry")), before);
        }
        assert_eq!(
            fixture
                .manager()
                .revive("worker", false, Some(&old.token), TIMEOUT)
                .unwrap()["revived"],
            true,
            "{interruption:?}"
        );
        let resumed = fixture.manager().load("worker").unwrap();
        assert_eq!(resumed.session_value, old.session_value);
        assert!(resumed.custom_process_identity.is_some());
        assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
        assert_eq!(
            *fixture.client.closed.lock().unwrap(),
            [old.pane_id.clone().unwrap()]
        );
    }
}

#[test]
fn revive_stale_session_exclusion_preserves_refusal_for_a_third_live_duplicate() {
    for workspace in ["workspace", "unrelated-workspace"] {
        let (fixture, old) = recoverable("muse", true);
        fixture
            .client
            .stale_reported_panes
            .lock()
            .unwrap()
            .insert(old.pane_id.clone().unwrap());
        super::super::revive::fail_once("launched");
        assert!(fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .is_err());
        let mut third = Fake::pane("third-live");
        third.tab_id = "third-tab".to_owned();
        third.workspace_id = workspace.to_owned();
        fixture.client.panes.lock().unwrap().push(third);
        fixture
            .client
            .duplicate_session
            .store(true, Ordering::Relaxed);
        let before = files(&fixture.root.join("registry"));
        let plan = fixture
            .manager()
            .revive("worker", true, None, TIMEOUT)
            .unwrap();
        assert_eq!(plan["action"], "blocked");
        assert!(
            plan["reason"].as_str().unwrap().contains("found 2"),
            "{plan}"
        );
        assert!(fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .is_err());
        assert_eq!(files(&fixture.root.join("registry")), before);
        assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        fixture
            .client
            .panes
            .lock()
            .unwrap()
            .retain(|pane| pane.pane_id != "third-live");
        fixture
            .client
            .duplicate_session
            .store(false, Ordering::Relaxed);
        assert_eq!(
            fixture
                .manager()
                .revive("worker", false, None, TIMEOUT)
                .unwrap()["revived"],
            true
        );
        assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
    }
}

#[test]
fn revive_stale_session_exclusion_requires_the_saved_death_and_shell_proofs() {
    for changed in ["alive", "unknown", "shell"] {
        let (fixture, old) = recoverable("muse", true);
        fixture
            .client
            .stale_reported_panes
            .lock()
            .unwrap()
            .insert(old.pane_id.clone().unwrap());
        super::super::revive::fail_once("launched");
        assert!(fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .is_err());
        match changed {
            "alive" => {
                fixture
                    .client
                    .dead_panes
                    .lock()
                    .unwrap()
                    .remove(old.pane_id.as_ref().unwrap());
            }
            "unknown" => fixture
                .client
                .liveness_unknown
                .store(true, Ordering::Relaxed),
            _ => {
                fixture
                    .client
                    .foreign_shell_identity
                    .lock()
                    .unwrap()
                    .starttime_ticks += 1
            }
        }
        let before = files(&fixture.root.join("registry"));
        assert_eq!(
            fixture
                .manager()
                .revive("worker", true, None, TIMEOUT)
                .unwrap()["action"],
            "blocked",
            "{changed}"
        );
        assert!(
            fixture
                .manager()
                .revive("worker", false, None, TIMEOUT)
                .is_err(),
            "{changed}"
        );
        assert_eq!(files(&fixture.root.join("registry")), before, "{changed}");
        assert_eq!(
            fixture.client.environments.lock().unwrap().len(),
            2,
            "{changed}"
        );
        assert!(
            fixture.client.closed.lock().unwrap().is_empty(),
            "{changed}"
        );
    }
}

#[test]
fn revive_rejects_a_different_reported_conversation_before_retirement() {
    let (fixture, old) = recoverable("claude", true);
    *fixture.client.session_override.lock().unwrap() =
        Some((Some("claude".to_owned()), Some("different".to_owned())));
    let old_bytes = fs::read(fixture.root.join("registry/worker/agent.json")).unwrap();
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .is_err());
    assert_eq!(
        fs::read(fixture.root.join("registry/worker/agent.json")).unwrap(),
        old_bytes
    );
    assert!(journal_path(&fixture, &old).exists());
    assert!(fixture.client.closed.lock().unwrap().is_empty());
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .is_err());
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
}

#[test]
fn revive_resumes_each_crash_boundary_without_allocating_again() {
    for point in [
        "launched",
        "ready",
        "stopped",
        "archived",
        "published",
        "closed",
    ] {
        let (fixture, old) = recoverable("claude", false);
        super::super::revive::fail_once(point);
        assert!(
            fixture
                .manager()
                .revive("worker", false, Some(&old.token), TIMEOUT)
                .is_err(),
            "{point}"
        );
        assert!(journal_path(&fixture, &old).exists(), "{point}");
        assert_eq!(
            fixture.client.environments.lock().unwrap().len(),
            2,
            "{point}"
        );
        let result = fixture
            .manager()
            .revive("worker", false, Some(&old.token), TIMEOUT)
            .unwrap_or_else(|error| panic!("{point}: {error}"));
        assert_eq!(result["revived"], true, "{point}");
        assert_eq!(
            fixture.client.environments.lock().unwrap().len(),
            2,
            "{point}"
        );
        assert!(!journal_path(&fixture, &old).exists(), "{point}");
    }
}

#[test]
fn revive_launch_once_latch_refuses_unanchored_interruption_and_gates_ordinary_controls() {
    let (fixture, old) = recoverable("claude", false);
    super::super::revive::fail_once("launching");
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .is_err());
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 1);
    assert!(fixture
        .manager()
        .get("worker")
        .unwrap_err()
        .to_string()
        .contains("revive"));
    assert!(fixture.manager().stop("worker").is_err());
    assert!(fixture
        .manager()
        .start("worker", &fixture.root, StartOptions::default())
        .is_err());
    let before = files(&fixture.root.join("registry"));
    let dry_run = fixture
        .manager()
        .revive("worker", true, Some(&old.token), TIMEOUT)
        .unwrap();
    assert_eq!(dry_run["action"], "blocked");
    assert!(dry_run["reason"]
        .as_str()
        .unwrap()
        .contains("no automatic relaunch"));
    assert_eq!(files(&fixture.root.join("registry")), before);
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .unwrap_err()
        .to_string()
        .contains("refusing another launch"));
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 1);
}

#[test]
fn revive_preserves_unlaunched_partial_setup_before_retrying_once() {
    for point in [
        "operation-created",
        "stage-created",
        "stopped-prepared",
        "candidate-prepared",
    ] {
        let (fixture, old) = recoverable("claude", false);
        super::super::revive::fail_once(point);
        assert!(
            fixture
                .manager()
                .revive("worker", false, None, TIMEOUT)
                .is_err(),
            "{point}"
        );
        assert!(!journal_path(&fixture, &old).exists(), "{point}");
        assert_eq!(
            fixture.client.environments.lock().unwrap().len(),
            1,
            "{point}"
        );
        let operation = stage_path(&fixture, &old).parent().unwrap().to_owned();
        let artifacts = files(&operation);
        let before = files(&fixture.root.join("registry"));
        assert_eq!(
            fixture
                .manager()
                .revive("worker", true, None, TIMEOUT)
                .unwrap()["action"],
            "revive",
            "{point}"
        );
        assert_eq!(files(&fixture.root.join("registry")), before, "{point}");
        assert_eq!(
            fixture
                .manager()
                .revive("worker", false, None, TIMEOUT)
                .unwrap()["revived"],
            true,
            "{point}"
        );
        let root = fixture.root.join("registry/.revives");
        let orphans: Vec<_> = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".orphan-")
            })
            .collect();
        assert_eq!(orphans.len(), 1, "{point}");
        assert_eq!(files(&orphans[0]), artifacts, "{point}");
        assert_eq!(
            fixture.client.environments.lock().unwrap().len(),
            2,
            "{point}"
        );
    }
}

#[test]
fn revive_refuses_orphaned_setup_with_runtime_claims_or_unrecognized_artifacts() {
    for modification in ["pane", "session", "unknown", "symlink", "hardlink"] {
        let (fixture, old) = recoverable("claude", false);
        super::super::revive::fail_once("candidate-prepared");
        assert!(fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .is_err());
        let stage = stage_path(&fixture, &old);
        let record_path = stage.join("agent.json");
        match modification {
            "pane" | "session" => {
                let mut candidate = agent::read_private_json(&record_path).unwrap();
                if modification == "pane" {
                    candidate["pane_id"] = json!("unproven-pane");
                } else {
                    candidate["session_agent"] = json!("claude");
                    candidate["session_value"] = old
                        .native_session
                        .as_ref()
                        .map_or(Value::Null, |native| json!(native.value));
                }
                agent::atomic_json(&record_path, &candidate).unwrap();
            }
            "unknown" => fs::write(stage.join("unrecognized"), "retained").unwrap(),
            "symlink" => {
                std::os::unix::fs::symlink(&record_path, stage.join(".agent.json-recovery-1-1-0"))
                    .unwrap()
            }
            _ => fs::hard_link(&record_path, stage.join(".agent.json-recovery-1-1-0")).unwrap(),
        }
        let before = files(&fixture.root.join("registry"));
        assert!(
            fixture
                .manager()
                .revive("worker", false, None, TIMEOUT)
                .is_err(),
            "{modification}"
        );
        assert_eq!(
            files(&fixture.root.join("registry")),
            before,
            "{modification}"
        );
        assert_eq!(
            fixture.client.environments.lock().unwrap().len(),
            1,
            "{modification}"
        );
    }
}

#[test]
fn revive_preserves_both_producers_private_setup_temporary_files() {
    let (fixture, old) = recoverable("claude", false);
    super::super::revive::fail_once("candidate-prepared");
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .is_err());
    let stage = stage_path(&fixture, &old);
    for name in [
        ".agent.json-recovery-17-123456789-0",
        ".agent.json-recovery-19-aabbccddeeff00112233445566778899",
    ] {
        let path = stage.join(name);
        fs::write(&path, b"retained temporary").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let operation = stage.parent().unwrap();
    let artifacts = files(operation);
    assert_eq!(
        fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .unwrap()["revived"],
        true
    );
    let orphan = fs::read_dir(fixture.root.join("registry/.revives"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".orphan-")
        })
        .unwrap();
    assert_eq!(files(&orphan), artifacts);
}

#[test]
fn revive_staged_pane_and_session_stay_reserved_across_archive_publication_gap() {
    let (fixture, old) = recoverable("codex", true);
    super::super::revive::fail_once("archived");
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .is_err());
    assert!(!fixture.root.join("registry/worker").exists());
    let candidate =
        agent::read_private_json(&stage_path(&fixture, &old).join("agent.json")).unwrap();
    let mut options = fixture.adopt_options();
    options.pane_id = candidate["pane_id"].as_str().unwrap().to_owned();
    let error = fixture
        .manager()
        .adopt("alias", options)
        .unwrap_err()
        .to_string();
    assert!(error.contains("already registered"), "{error}");
    assert!(!fixture.root.join("registry/alias").exists());
    assert!(fixture
        .manager()
        .start(
            "duplicate",
            &fixture.root,
            StartOptions {
                resume: old
                    .native_session
                    .as_ref()
                    .map(|native| native.value.clone()),
                ..StartOptions::default()
            }
        )
        .unwrap_err()
        .to_string()
        .contains("already registered"));
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
    let before = files(&fixture.root.join("registry"));
    assert_eq!(
        fixture
            .manager()
            .revive("worker", true, None, TIMEOUT)
            .unwrap()["action"],
        "recover"
    );
    assert_eq!(files(&fixture.root.join("registry")), before);
    assert_eq!(
        fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .unwrap()["revived"],
        true
    );
}

#[test]
fn revive_recovers_failed_or_starting_candidate_only_with_its_saved_live_anchor() {
    for lifecycle in ["starting", "launch_failed"] {
        let (fixture, old) = recoverable("claude", false);
        super::super::revive::fail_once("launched");
        assert!(fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .is_err());
        let path = stage_path(&fixture, &old).join("agent.json");
        let mut candidate = agent::read_private_json(&path).unwrap();
        candidate["lifecycle"] = json!(lifecycle);
        candidate["error"] = json!("injected failure after pinning");
        agent::atomic_json(&path, &candidate).unwrap();
        assert_eq!(
            fixture
                .manager()
                .revive("worker", false, None, TIMEOUT)
                .unwrap()["revived"],
            true
        );
        assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
    }
}

#[test]
fn revive_lost_close_acknowledgment_reconciles_only_explicit_absence() {
    let (fixture, old) = recoverable("claude", false);
    fixture
        .client
        .fail_after_close
        .store(true, Ordering::Relaxed);
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .unwrap_err()
        .to_string()
        .contains("close is uncertain"));
    assert_eq!(
        agent::read_private_json(&journal_path(&fixture, &old)).unwrap()["phase"],
        "published"
    );
    assert_eq!(
        fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .unwrap()["revived"],
        true
    );
    assert_eq!(fixture.client.closed.lock().unwrap().len(), 1);
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
}

#[test]
fn revive_never_adopts_a_replacement_idle_shell_as_cleanup_authority() {
    let (fixture, old) = recoverable("claude", false);
    fixture.client.fail_close.store(true, Ordering::Relaxed);
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .is_err());
    fixture.client.fail_close.store(false, Ordering::Relaxed);
    fixture
        .client
        .foreign_shell_identity
        .lock()
        .unwrap()
        .starttime_ticks += 1;
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .unwrap_err()
        .to_string()
        .contains("shell generation changed"));
    assert!(journal_path(&fixture, &old).exists());
    assert!(fixture.client.closed.lock().unwrap().is_empty());
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
}

#[test]
fn revive_published_retry_does_not_require_the_new_harness_to_still_run() {
    let (fixture, old) = recoverable("claude", false);
    fixture.client.fail_close.store(true, Ordering::Relaxed);
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .is_err());
    let new: AgentRecord = serde_json::from_value(
        agent::read_private_json(&fixture.root.join("registry/worker/agent.json")).unwrap(),
    )
    .unwrap();
    kill_owned(&fixture, &new);
    fixture.client.fail_close.store(false, Ordering::Relaxed);
    assert_eq!(
        fixture
            .manager()
            .revive("worker", false, Some(&old.token), TIMEOUT)
            .unwrap()["revived"],
        true
    );
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
}

#[test]
fn revive_published_retry_retains_the_journal_if_a_stage_reappears() {
    let (fixture, old) = recoverable("claude", false);
    super::super::revive::fail_once("published");
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .is_err());
    let stage = stage_path(&fixture, &old);
    DirBuilder::new().mode(0o700).create(&stage).unwrap();
    fs::copy(
        fixture.root.join("registry/worker/agent.json"),
        stage.join("agent.json"),
    )
    .unwrap();
    let before = files(&fixture.root.join("registry"));
    assert_eq!(
        fixture
            .manager()
            .revive("worker", true, None, TIMEOUT)
            .unwrap()["action"],
        "blocked"
    );
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .unwrap_err()
        .to_string()
        .contains("conflicting staged generation"));
    assert_eq!(files(&fixture.root.join("registry")), before);
    assert!(journal_path(&fixture, &old).exists());
    assert!(fixture.client.closed.lock().unwrap().is_empty());
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
}

#[test]
fn revive_detects_changed_old_bytes_and_staged_directory_generation() {
    for mutation in ["old-bytes", "new-directory", "new-policy", "stopped-bytes"] {
        let (fixture, old) = recoverable("claude", false);
        super::super::revive::fail_once("ready");
        assert!(fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .is_err());
        match mutation {
            "old-bytes" => {
                let path = fixture.root.join("registry/worker/agent.json");
                let mut value = agent::read_private_json(&path).unwrap();
                value["opaque"] = json!("changed");
                agent::atomic_json(&path, &value).unwrap();
            }
            "new-directory" => {
                let path = stage_path(&fixture, &old);
                let bytes = fs::read(path.join("agent.json")).unwrap();
                fs::rename(&path, path.with_file_name("replaced-new")).unwrap();
                agent::create_private_directory(&path, "replacement test candidate", false, false)
                    .unwrap();
                fs::write(path.join("agent.json"), bytes).unwrap();
                fs::set_permissions(path.join("agent.json"), fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            "new-policy" => {
                let path = stage_path(&fixture, &old).join("agent.json");
                let mut value = agent::read_private_json(&path).unwrap();
                value["arguments"] = json!(["--resume", "different"]);
                agent::atomic_json(&path, &value).unwrap();
            }
            _ => {
                let path = stage_path(&fixture, &old)
                    .parent()
                    .unwrap()
                    .join("stopped.json");
                let mut value = agent::read_private_json(&path).unwrap();
                value["opaque"] = json!("changed");
                agent::atomic_json(&path, &value).unwrap();
            }
        }
        assert!(
            fixture
                .manager()
                .revive("worker", false, None, TIMEOUT)
                .is_err(),
            "{mutation}"
        );
        assert!(journal_path(&fixture, &old).exists());
        assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }
}

#[test]
fn pending_failed_candidate_reserves_its_pane_and_native_conversation() {
    let (fixture, old) = recoverable("claude", false);
    super::super::revive::fail_once("launched");
    assert!(fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .is_err());
    let path = stage_path(&fixture, &old).join("agent.json");
    let mut candidate = agent::read_private_json(&path).unwrap();
    candidate["lifecycle"] = json!("launch_failed");
    agent::atomic_json(&path, &candidate).unwrap();
    let pane = candidate["pane_id"].as_str().unwrap();
    assert_eq!(
        fixture
            .manager()
            .claim_owner(pane, candidate["terminal_id"].as_str(), Some("peer"))
            .unwrap(),
        Some("worker".to_owned())
    );
    let native = old.native_session.unwrap();
    assert_eq!(
        fixture
            .manager()
            .identity_owner(&native.agent, &native.value, Some("peer"))
            .unwrap()
            .unwrap()
            .name,
        "worker"
    );
}

#[test]
fn revive_all_snapshots_tokens_and_continues_independent_blocked_records() {
    let (fixture, old) = recoverable("claude", false);
    let manager = fixture.manager();
    let mut peer = old.clone();
    peer.name = "peer".to_owned();
    peer.token = "different-generation".to_owned();
    peer.native_session = None;
    peer.session_agent = None;
    peer.session_value = None;
    peer.harness_identity = None;
    peer.anchor_rule = None;
    peer.pane_id = Some("unknown-pane".to_owned());
    peer.tab_id = Some("unknown-tab".to_owned());
    peer.terminal_id = Some("unknown-terminal".to_owned());
    let directory = fixture.root.join("registry/peer");
    agent::create_private_directory(&directory, "test blocked peer", false, false).unwrap();
    manager.save(&peer).unwrap();
    let plan = manager.revive_all(true, TIMEOUT).unwrap();
    assert_eq!(plan["blocked"], 1);
    assert_eq!(plan["revived"], 0);
    let result = manager.revive_all(false, TIMEOUT).unwrap();
    assert_eq!(result["blocked"], 1);
    assert_eq!(result["revived"], 1);
    assert_eq!(result["agents"].as_array().unwrap().len(), 2);
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
}

fn profile_fixture() -> (Fixture, AgentRecord, PathBuf) {
    let fixture = Fixture::new();
    fixture
        .client
        .fresh_presentations
        .store(true, Ordering::Relaxed);
    let output = std::process::Command::new("/usr/bin/git")
        .args(["init", "--quiet"])
        .current_dir(&fixture.root)
        .output()
        .unwrap();
    assert!(output.status.success());
    fs::write(fixture.root.join(".gitignore"), ".agentctl/\n").unwrap();
    let directory = fixture.root.join(".agentctl");
    agent::create_private_directory(&directory, "test profile config", false, false).unwrap();
    let path = directory.join("profiles.json");
    agent::atomic_json(&path, &json!({"schema":"agentctl-profiles/v1","profiles":{
        "claude-recovery": {"harness":"claude","mode":"interactive","model":"opus","reasoning_effort":"high",
            "argv":["--allowedTools","Read"], "env":{"RECOVERY_MODE":"profile-private-value"}}
    }})).unwrap();
    fixture
        .manager()
        .start_with_reasoning_effort(
            "worker",
            &fixture.root,
            "high",
            StartOptions {
                harness: "claude".to_owned(),
                profile: Some("claude-recovery".to_owned()),
                model: Some("opus".to_owned()),
                harness_args: vec!["--allowedTools".to_owned(), "Read".to_owned()],
                environment: vec!["RECOVERY_MODE=profile-private-value".to_owned()],
                ..StartOptions::default()
            },
        )
        .unwrap();
    let old = fixture.manager().load("worker").unwrap();
    kill_owned(&fixture, &old);
    (fixture, old, path)
}

#[test]
fn revive_loads_profile_environment_privately_and_keeps_its_policy() {
    let (fixture, old, _) = profile_fixture();
    let before = files(&fixture.root.join("registry"));
    let plan = fixture
        .manager()
        .revive("worker", true, None, TIMEOUT)
        .unwrap();
    assert_eq!(plan["action"], "revive");
    assert!(!plan.to_string().contains("profile-private-value"));
    assert_eq!(files(&fixture.root.join("registry")), before);
    let result = fixture
        .manager()
        .revive("worker", false, None, TIMEOUT)
        .unwrap();
    assert_eq!(result["profile"], "claude-recovery");
    assert!(!result.to_string().contains("profile-private-value"));
    let new = fixture.manager().load("worker").unwrap();
    assert_eq!(&new.arguments[2..], &old.arguments[2..]);
    assert_eq!(
        fixture.client.environments.lock().unwrap()[1],
        ["RECOVERY_MODE=profile-private-value"]
    );
}

#[test]
fn revive_refuses_profile_model_effort_argv_and_environment_name_drift() {
    for field in ["model", "effort", "argv", "environment"] {
        let (fixture, _, path) = profile_fixture();
        let mut config = agent::read_private_json(&path).unwrap();
        let profile = &mut config["profiles"]["claude-recovery"];
        match field {
            "model" => profile["model"] = json!("different"),
            "effort" => profile["reasoning_effort"] = json!("medium"),
            "argv" => profile["argv"] = json!(["--allowedTools", "Read,Write"]),
            _ => profile["env"] = json!({"DIFFERENT_MODE":"profile-private-value"}),
        }
        agent::atomic_json(&path, &config).unwrap();
        let plan = fixture
            .manager()
            .revive("worker", true, None, TIMEOUT)
            .unwrap();
        assert_eq!(plan["action"], "blocked", "{field}");
        assert!(!plan.to_string().contains("profile-private-value"));
        assert!(fixture
            .manager()
            .revive("worker", false, None, TIMEOUT)
            .is_err());
        assert_eq!(fixture.client.environments.lock().unwrap().len(), 1);
    }
}

#[test]
fn revive_checks_recorded_slot_path_and_isolation_in_read_only_preflight() {
    for field in ["path", "isolation"] {
        let (fixture, mut old) = recoverable("claude", false);
        old.slot = Some("slot_name".to_owned());
        old.slot_project = Some(fixture.root.display().to_string());
        old.slot_isolation = Some("userns".to_owned());
        fixture.manager().save(&old).unwrap();
        let bin = fixture.root.join("wrkslots-test");
        let output_path = if field == "path" {
            fixture.root.join("changed")
        } else {
            fixture.root.clone()
        };
        let output = json!({"command":"exec owner-policy", "slot_path":output_path, "isolation":if field == "path" { "userns" } else { "cgroup" }}).to_string();
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' '{}'\n",
            output.replace('\'', "'\\''")
        );
        fs::write(&bin, script).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
        let before = files(&fixture.root.join("registry"));
        let mut manager = fixture.manager();
        manager.revive_wrkslots_executable = Some(bin);
        let plan = manager.revive("worker", true, None, TIMEOUT).unwrap();
        assert_eq!(plan["action"], "blocked");
        assert_eq!(files(&fixture.root.join("registry")), before);
        assert_eq!(fixture.client.environments.lock().unwrap().len(), 1);
    }
}

#[test]
fn revive_reuses_the_recorded_slot_without_allocation_or_rebinding() {
    let (fixture, mut old) = recoverable("claude", false);
    old.slot = Some("existing_slot".to_owned());
    old.slot_project = Some(fixture.root.display().to_string());
    old.slot_isolation = Some("userns".to_owned());
    fixture.manager().save(&old).unwrap();
    let output = json!({"command":"exec preserved-owner-policy", "slot_path":fixture.root, "isolation":"userns"}).to_string();
    let bin = fixture.root.join("wrkslots-test");
    fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{}'\n",
            output.replace('\'', "'\\''")
        ),
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    let mut manager = fixture.manager();
    manager.revive_wrkslots_executable = Some(bin);
    let before = files(&fixture.root.join("registry"));
    assert_eq!(
        manager.revive("worker", true, None, TIMEOUT).unwrap()["action"],
        "revive"
    );
    assert_eq!(files(&fixture.root.join("registry")), before);
    assert!(fixture.client.slot_commands.lock().unwrap().is_empty());
    assert_eq!(
        manager.revive("worker", false, None, TIMEOUT).unwrap()["revived"],
        true
    );
    let new = manager.load("worker").unwrap();
    assert_eq!(new.slot, old.slot);
    assert_eq!(new.slot_project, old.slot_project);
    assert_eq!(new.slot_isolation, old.slot_isolation);
    assert_eq!(
        *fixture.client.slot_commands.lock().unwrap(),
        [(
            new.pane_id.unwrap(),
            "exec preserved-owner-policy".to_owned()
        )]
    );
    assert_eq!(fixture.client.environments.lock().unwrap().len(), 2);
}
