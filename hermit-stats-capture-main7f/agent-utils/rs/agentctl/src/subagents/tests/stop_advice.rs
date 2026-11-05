use super::*;

fn retry_options(recovery: &RecoveryAction) -> StopOptions {
    let RecoveryAction::Stop {
        token,
        record_sha256,
        skip_cloud_halt,
        ..
    } = recovery
    else {
        panic!("expected a bound stop recovery action");
    };
    StopOptions {
        expected_token: Some(token.clone()),
        recover_legacy_adoption: record_sha256.is_some(),
        retire_dead_adoption: false,
        expected_record_sha256: record_sha256.clone(),
        skip_cloud_halt: *skip_cloud_halt,
    }
}

#[test]
fn managed_dead_advice_is_bound_to_the_original_generation_and_can_be_executed() {
    let fixture = Fixture::new();
    let (pane, token) = fixture.make_managed_dead();
    let path = fixture.root.join("registry/worker/agent.json");
    let original = fs::read(&path).unwrap();
    let failure = fixture
        .manager()
        .advised_stop_with_options("worker", StopOptions::default())
        .unwrap_err();
    assert_eq!(failure.error.exit_code(), 75);
    assert!(failure.to_string().contains("requires --expected-token"));
    assert_eq!(
        failure.recovery,
        RecoveryAction::Stop {
            name: "worker".to_owned(),
            token: token.clone(),
            record_sha256: None,
            skip_cloud_halt: false,
        }
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(fixture.client.closed.lock().unwrap().is_empty());

    let mut replacement: Value = serde_json::from_slice(&original).unwrap();
    replacement["token"] = json!("replacement-generation");
    agent::atomic_json(&path, &replacement).unwrap();
    let replacement_bytes = fs::read(&path).unwrap();
    let refused = fixture
        .manager()
        .advised_stop_with_options("worker", retry_options(&failure.recovery))
        .unwrap_err();
    assert_eq!(refused.recovery, RecoveryAction::Doctor);
    assert_eq!(refused.error.exit_code(), 75);
    assert!(refused.to_string().contains("replaced"));
    assert_eq!(fs::read(&path).unwrap(), replacement_bytes);
    assert!(fixture.client.closed.lock().unwrap().is_empty());

    fs::write(&path, &original).unwrap();
    let result = fixture
        .manager()
        .advised_stop_with_options("worker", retry_options(&failure.recovery))
        .unwrap();
    assert_eq!(result["managed_dead"], true);
    assert_eq!(*fixture.client.closed.lock().unwrap(), [pane]);
    assert!(!path.exists());
}

#[test]
fn legacy_advice_captures_exact_bytes_and_its_retry_preserves_the_foreign_runtime() {
    let fixture = Fixture::new();
    let (token, digest, original) = fixture.make_legacy_dead();
    let path = fixture.root.join("registry/foreign/agent.json");
    let failure = fixture
        .manager()
        .advised_stop_with_options("foreign", StopOptions::default())
        .unwrap_err();
    assert_eq!(failure.error.exit_code(), 75);
    assert!(failure.to_string().contains("legacy record"));
    assert_eq!(
        failure.recovery,
        RecoveryAction::Stop {
            name: "foreign".to_owned(),
            token,
            record_sha256: Some(digest.clone()),
            skip_cloud_halt: false,
        }
    );
    assert_eq!(fs::read(&path).unwrap(), original);

    let changed = [original.as_slice(), b" \n"].concat();
    fs::write(&path, &changed).unwrap();
    let refused = fixture
        .manager()
        .advised_stop_with_options("foreign", retry_options(&failure.recovery))
        .unwrap_err();
    assert_eq!(refused.recovery, RecoveryAction::Doctor);
    assert_eq!(refused.error.exit_code(), 75);
    assert_eq!(fs::read(&path).unwrap(), changed);
    assert!(fixture.client.closed.lock().unwrap().is_empty());

    fs::write(&path, &original).unwrap();
    let result = fixture
        .manager()
        .advised_stop_with_options("foreign", retry_options(&failure.recovery))
        .unwrap();
    assert_eq!(result["record_sha256"], digest);
    assert_eq!(result["runtime_preserved"], true);
    let archive = PathBuf::from(result["archive"].as_str().unwrap());
    assert_eq!(fs::read(archive.join("agent.json")).unwrap(), original);
    assert_eq!(*fixture.client.panes.lock().unwrap(), [Fake::pane("owned")]);
    assert!(fixture.client.closed.lock().unwrap().is_empty());
}

#[test]
fn unproved_legacy_recovery_uses_doctor_without_capturing_a_new_shell_identity() {
    for case in ["null-key", "live-agent", "busy-shell"] {
        let fixture = Fixture::new();
        fixture.make_legacy_dead();
        let path = fixture.root.join("registry/foreign/agent.json");
        match case {
            "null-key" => {
                let mut document = agent::read_private_json(&path).unwrap();
                document["foreign_shell_identity"] = Value::Null;
                agent::atomic_json(&path, &document).unwrap();
            }
            "live-agent" => {
                fixture.client.started.store(true, Ordering::Relaxed);
                fixture.client.report_session.store(true, Ordering::Relaxed);
            }
            "busy-shell" => fixture
                .client
                .custom_at_idle_shell
                .store(false, Ordering::Relaxed),
            _ => unreachable!(),
        }
        let original = fs::read(&path).unwrap();
        let failure = fixture
            .manager()
            .advised_stop_with_options("foreign", StopOptions::default())
            .unwrap_err();
        assert_eq!(failure.recovery, RecoveryAction::Doctor, "{case}");
        assert_eq!(failure.error.exit_code(), 75, "{case}");
        assert_eq!(fs::read(&path).unwrap(), original, "{case}");
        assert!(fixture.client.closed.lock().unwrap().is_empty(), "{case}");
        assert!(!fixture.root.join("registry/archive").exists(), "{case}");
    }
}

#[test]
fn changed_or_untrusted_snapshot_cannot_supply_a_replacement_token_in_advice() {
    for case in ["token", "task", "corrupt", "hardlink", "directory"] {
        let fixture = Fixture::new();
        fixture.make_managed_dead();
        let manager = fixture.manager();
        let record = manager.load("worker").unwrap();
        let pinned = manager.pinned_agent_directory("worker").unwrap();
        let path = fixture.root.join("registry/worker/agent.json");
        let mut document = agent::read_private_json(&path).unwrap();
        match case {
            "token" => {
                document["token"] = json!("replacement-generation");
                agent::atomic_json(&path, &document).unwrap();
            }
            "task" => {
                document["goal"] = json!("Changed after the operation loaded its record");
                agent::atomic_json(&path, &document).unwrap();
            }
            "corrupt" => fs::write(&path, b"not json").unwrap(),
            "hardlink" => fs::hard_link(&path, fixture.root.join("record-link")).unwrap(),
            "directory" => {
                fs::rename(
                    fixture.root.join("registry/worker"),
                    fixture.root.join("displaced-record"),
                )
                .unwrap();
                agent::create_private_directory(
                    &fixture.root.join("registry/worker"),
                    "replacement test record",
                    false,
                    false,
                )
                .unwrap();
                document["token"] = json!("replacement-generation");
                agent::atomic_json(&path, &document).unwrap();
            }
            _ => unreachable!(),
        }
        let mut recovery = RecoveryAction::Doctor;
        let error = manager
            .retire_managed_dead_locked(&record, &pinned, &StopOptions::default(), &mut recovery)
            .unwrap_err();
        assert_eq!(recovery, RecoveryAction::Doctor, "{case}: {error}");
        assert_eq!(error.exit_code(), 75, "{case}");
        assert!(fixture.client.closed.lock().unwrap().is_empty(), "{case}");
        assert!(!fixture.root.join("registry/archive").exists(), "{case}");
    }
}

#[test]
fn delayed_pending_rename_and_move_advice_only_inspects_a_replacement() {
    for operation in ["rename", "move"] {
        let fixture = Fixture::new();
        fixture
            .client
            .fresh_presentations
            .store(true, Ordering::Relaxed);
        fixture.start(None);
        let old = fixture.manager().load("worker").unwrap();
        if operation == "rename" {
            fixture.client.fail_rename_tab.store(true, Ordering::SeqCst);
            fixture.manager().rename("worker", "reviewer").unwrap_err();
            fixture
                .client
                .fail_rename_tab
                .store(false, Ordering::SeqCst);
        } else {
            fixture
                .manager()
                .write_move_intent(&old, "project-workspace")
                .unwrap();
        }
        let failure = fixture
            .manager()
            .advised_stop_with_options("worker", StopOptions::default())
            .unwrap_err();
        assert_eq!(failure.error.exit_code(), 75);
        assert!(!failure.to_string().contains("rerun"));
        match &failure.recovery {
            RecoveryAction::Rename {
                old: name,
                new,
                token,
            } if operation == "rename" => {
                assert_eq!(name, "worker");
                assert_eq!(new, "reviewer");
                assert_eq!(token, &old.token);
                assert!(fixture
                    .manager()
                    .status("worker")
                    .unwrap_err()
                    .to_string()
                    .contains("rerun"));
                fixture.manager().rename("worker", "reviewer").unwrap();
                fixture.manager().stop("reviewer").unwrap();
            }
            RecoveryAction::Move { name, token } if operation == "move" => {
                assert_eq!(name, "worker");
                assert_eq!(token, &old.token);
                assert!(fixture.manager().status("worker").unwrap()["probe_error"]
                    .as_str()
                    .unwrap()
                    .contains("rerun"));
                fixture
                    .manager()
                    .with_project_workspace(Some("project-agents"))
                    .move_to_project_workspace("worker")
                    .unwrap();
                fixture.manager().stop("worker").unwrap();
            }
            action => panic!("unexpected {operation} action: {action:?}"),
        }
        fixture.start(None);
        let replacement = fixture.manager().load("worker").unwrap();
        assert_ne!(replacement.token, old.token);
        let path = fixture.root.join("registry/worker/agent.json");
        let bytes = fs::read(&path).unwrap();
        let panes = fixture.client.panes.lock().unwrap().clone();
        let labels = fixture.client.labels.lock().unwrap().clone();
        let names = fixture.client.herdr_names.lock().unwrap().clone();
        let moves = fixture.client.moves.lock().unwrap().clone();
        let closed = fixture.client.closed.lock().unwrap().clone();
        let context = StopContext::new(&fixture.root.join("registry"), Path::new("literal-herdr"));
        assert!(context.command(&failure.recovery).ends_with(" doctor"));
        fixture.manager().doctor(false).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(*fixture.client.panes.lock().unwrap(), panes);
        assert_eq!(*fixture.client.labels.lock().unwrap(), labels);
        assert_eq!(*fixture.client.herdr_names.lock().unwrap(), names);
        assert_eq!(*fixture.client.moves.lock().unwrap(), moves);
        assert_eq!(*fixture.client.closed.lock().unwrap(), closed);
    }
}

#[test]
fn pending_revive_advice_keeps_the_journal_token_before_and_after_publication() {
    for point in ["ready", "published"] {
        let fixture = Fixture::new();
        fixture
            .client
            .fresh_presentations
            .store(true, Ordering::Relaxed);
        fixture.start(None);
        let old = fixture.manager().load("worker").unwrap();
        fixture
            .client
            .dead_panes
            .lock()
            .unwrap()
            .insert(old.pane_id.clone().unwrap());
        fixture.client.started.store(false, Ordering::Relaxed);
        fixture.client.custom_alive.store(false, Ordering::Relaxed);
        super::super::revive::fail_once(point);
        fixture
            .manager()
            .revive("worker", false, Some(&old.token), Duration::from_secs(30))
            .unwrap_err();
        let failure = fixture
            .manager()
            .advised_stop_with_options("worker", StopOptions::default())
            .unwrap_err();
        assert_eq!(failure.error.exit_code(), 75);
        assert_eq!(failure.to_string(), "revive of 'worker' is incomplete");
        assert_eq!(
            failure.recovery,
            RecoveryAction::Revive {
                name: "worker".to_owned(),
                token: old.token.clone()
            }
        );
        assert!(fixture
            .manager()
            .status("worker")
            .unwrap_err()
            .to_string()
            .contains("rerun"));
        let mismatch = fixture
            .manager()
            .advised_stop_with_options(
                "worker",
                StopOptions {
                    expected_token: Some("replacement-generation".to_owned()),
                    ..StopOptions::default()
                },
            )
            .unwrap_err();
        assert_eq!(mismatch.recovery, RecoveryAction::Doctor);
        if point == "published" {
            assert_ne!(
                fixture.manager().read_record("worker").unwrap().token,
                old.token
            );
        }
    }
}

#[test]
fn unavailable_and_unsupported_stop_refusals_keep_their_codes_and_use_doctor() {
    for case in ["unknown", "unavailable", "headless"] {
        let fixture = Fixture::new();
        if case != "unknown" {
            fixture.start(None);
        }
        if case == "unavailable" {
            fixture.client.fail_panes.store(true, Ordering::Relaxed);
        } else if case == "headless" {
            let path = fixture.root.join("registry/worker/agent.json");
            let mut document = agent::read_private_json(&path).unwrap();
            document["adapter"] = json!("turn-runner");
            document["mode"] = json!("headless");
            // A runtime home that does not exist cannot prove its runner dead.
            document["runtime_home"] = json!(fixture.root.join("missing-runtime"));
            agent::atomic_json(&path, &document).unwrap();
        }
        let failure = fixture
            .manager()
            .advised_stop_with_options("worker", StopOptions::default())
            .unwrap_err();
        assert_eq!(failure.recovery, RecoveryAction::Doctor, "{case}");
        assert_eq!(
            failure.error.exit_code(),
            if case == "unknown" { 75 } else { 69 },
            "{case}"
        );
        assert!(!stop_reason(&failure).is_empty(), "{case}");
        assert!(fixture.client.closed.lock().unwrap().is_empty(), "{case}");
    }
}
