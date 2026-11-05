//! Nested agentctl-session/v2 and v3 rows: decode, write-back, refusals and anchor migration.

// Nine cases follow the other edition's nested-record tests in py/tests/test_herdr_subagents.py
// one for one. The v2 bind-session, write-back, list, stop, rename, anchor, schema and revive
// cases cover what those tests do not.
use super::*;

fn record_path(fixture: &Fixture) -> PathBuf {
    fixture.root.join("registry/worker/agent.json")
}

fn stored(fixture: &Fixture) -> Value {
    agent::read_private_json(&record_path(fixture)).unwrap()
}

fn store(fixture: &Fixture, document: &Value) {
    agent::atomic_json(&record_path(fixture), document).unwrap();
}

/// Encode one flat record in the canonical launch-only v2 shape, as `_nested_v2_record` does.
fn nested_v2_record(flat: &Value) -> Value {
    let mut argv = vec![flat["harness"].clone()];
    argv.extend(flat["arguments"].as_array().unwrap().iter().cloned());
    json!({
        "schema": "agentctl-session/v2",
        "name": flat["name"],
        "token": flat["token"],
        "created_at": flat["created_at"],
        "lifecycle": flat["lifecycle"],
        "launch": {
            "schema": "agentctl-launch/v1",
            "harness": flat["harness"],
            "cwd": flat["cwd"],
            "adapter": flat["adapter"],
            "mode": flat["mode"],
            "backend": flat["backend"],
            "model": flat["model"],
            "resume": flat["resume"],
            "profile": null,
            "argv": argv,
            "environment_names": [],
            "runtime_home": flat["runtime_home"],
            "runtime_ownership": "owned",
            "executable": null,
        },
        "workspace_id": flat["workspace_id"],
        "tab_id": flat["tab_id"],
        "pane_id": flat["pane_id"],
        "session_agent": flat["session_agent"],
        "session_value": flat["session_value"],
        "startup_warning": flat["startup_warning"],
        "effective_reasoning_effort": flat["effective_reasoning_effort"],
        "error": flat["error"],
        "goal": flat["goal"],
        "goal_delivery": flat["goal_delivery"],
        "goal_session_id": flat["goal_session_id"],
        "goal_command": flat["goal_command"],
        "goal_messages": flat["goal_messages"],
        "goal_message_id": flat["goal_message_id"],
        "paused": flat["paused"],
        "pane_reported_by_agentctl": flat["pane_reported_by_agentctl"],
        "custom_process_identity": flat["custom_process_identity"],
        "foreign_shell_identity": flat["foreign_shell_identity"],
        "runner_identity": null,
        "extensions": {},
    })
}

/// Turn a v2 row into v3: one native session object and one goal object.
fn as_v3(mut nested: Value, native_session: Value) -> Value {
    let row = nested.as_object_mut().unwrap();
    row.insert("schema".to_owned(), json!("agentctl-session/v3"));
    row.remove("session_agent");
    row.remove("session_value");
    row.insert("native_session".to_owned(), native_session);
    let goal = json!({
        "schema": "agentctl-goal/v1",
        "objective": row.remove("goal").unwrap(),
        "message_id": row.remove("goal_message_id").unwrap(),
        "native_command": row.remove("goal_command").unwrap(),
    });
    row.insert("goal".to_owned(), goal);
    for key in ["goal_delivery", "goal_session_id", "goal_messages"] {
        row.remove(key);
    }
    nested
}

fn started_nested(fixture: &Fixture) -> Value {
    fixture.start(None);
    let nested = nested_v2_record(&stored(fixture));
    store(fixture, &nested);
    nested
}

#[test]
fn launch_only_v2_record_is_read_and_rewritten_without_flat_duplicates() {
    let fixture = Fixture::new();
    let nested = started_nested(&fixture);

    let record = fixture.manager().read_record("worker").unwrap();
    assert_eq!(record.harness, "codex");
    assert_eq!(record.arguments, ["--no-alt-screen"]);
    assert_eq!(
        fixture.manager().status("worker").unwrap()["launch"],
        nested["launch"]
    );
    assert_eq!(
        fixture.manager().pause("worker", true).unwrap()["paused"],
        true
    );

    let stored = stored(&fixture);
    assert_eq!(stored["schema"], "agentctl-session/v2");
    assert_eq!(stored["launch"], nested["launch"]);
    for duplicate in ["harness", "cwd", "adapter", "mode", "backend", "arguments"] {
        assert!(stored.get(duplicate).is_none(), "{duplicate}");
    }
}

#[test]
fn launch_only_v3_record_decodes_nested_goal_and_native_session() {
    let fixture = Fixture::new();
    let flat = {
        fixture.start(None);
        stored(&fixture)
    };
    let session_value = flat["session_value"].as_str().unwrap().to_owned();
    let nested = as_v3(
        nested_v2_record(&flat),
        json!({
            "schema": "agentctl-native-session/v1",
            "agent": flat["session_agent"],
            "value": session_value,
            "source": "observed",
        }),
    );
    store(&fixture, &nested);

    let record = fixture.manager().read_record("worker").unwrap();
    assert_eq!(record.harness, "codex");
    assert_eq!(record.session_agent.as_deref(), Some("codex"));
    assert_eq!(
        record.session_value.as_deref(),
        Some(session_value.as_str())
    );
    assert_eq!(record.document().unwrap(), nested);
}

#[test]
fn v2_observed_and_asserted_session_fields_round_trip_separately() {
    for source in ["observed", "asserted"] {
        let fixture = Fixture::new();
        fixture.start(None);
        let mut nested = nested_v2_record(&stored(&fixture));
        if source == "observed" {
            nested["session_agent"] = json!("codex");
            nested["session_value"] = json!("observed-session");
            nested["goal_session_id"] = Value::Null;
        } else {
            nested["session_agent"] = Value::Null;
            nested["session_value"] = Value::Null;
            nested["goal_session_id"] = json!("asserted-session");
        }
        store(&fixture, &nested);

        let record = fixture.manager().read_record("worker").unwrap();
        assert_eq!(record.document().unwrap(), nested, "{source}");
        let target = record.target().unwrap();
        if source == "observed" {
            assert_eq!(target.session_agent.as_deref(), Some("codex"));
            assert_eq!(target.session_value.as_deref(), Some("observed-session"));
        } else {
            assert_eq!(target.session_agent, None);
            assert_eq!(target.session_value, None);
        }
    }
}

#[test]
fn v3_native_session_source_controls_live_routing_identity() {
    for (source, expected_status) in [("asserted", "idle"), ("observed", "unknown")] {
        let fixture = Fixture::new();
        fixture.start(None);
        let nested = as_v3(
            nested_v2_record(&stored(&fixture)),
            json!({
                "schema": "agentctl-native-session/v1",
                "agent": "codex",
                "value": "asserted-session",
                "source": source,
            }),
        );
        store(&fixture, &nested);
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);

        let status = fixture.manager().status("worker").unwrap();
        assert_eq!(
            status["agent_status"], expected_status,
            "{source}: {status}"
        );
        if source == "asserted" {
            assert_eq!(status["probe_error"], Value::Null);
        } else {
            assert!(status["probe_error"]
                .as_str()
                .unwrap()
                .contains("expected exactly one live pane"));
        }
    }
}

#[test]
fn v3_bind_session_persists_one_asserted_native_authority() {
    let fixture = Fixture::new();
    fixture.start(None);
    let nested = as_v3(nested_v2_record(&stored(&fixture)), Value::Null);
    store(&fixture, &nested);
    fixture
        .client
        .report_session
        .store(false, Ordering::Relaxed);

    let bound = fixture
        .manager()
        .bind_session("worker", "manual-session", None)
        .unwrap();
    assert_eq!(bound["source"], "explicit");
    assert_eq!(
        stored(&fixture)["native_session"],
        json!({
            "schema": "agentctl-native-session/v1",
            "agent": "codex",
            "value": "manual-session",
            "source": "asserted",
        })
    );
    let reloaded = fixture.manager().read_record("worker").unwrap();
    assert_eq!(reloaded.target().unwrap().session_agent, None);
    assert_eq!(reloaded.target().unwrap().session_value, None);
}

#[test]
fn v2_bind_session_keeps_only_the_asserted_goal_session() {
    let fixture = Fixture::new();
    fixture.start(None);
    let mut nested = nested_v2_record(&stored(&fixture));
    nested["session_agent"] = Value::Null;
    nested["session_value"] = Value::Null;
    store(&fixture, &nested);
    fixture
        .client
        .report_session
        .store(false, Ordering::Relaxed);

    fixture
        .manager()
        .bind_session("worker", "manual-session", None)
        .unwrap();
    let stored = stored(&fixture);
    assert_eq!(stored["schema"], "agentctl-session/v2");
    assert_eq!(stored["goal_session_id"], "manual-session");
    assert_eq!(stored["session_value"], Value::Null);
    assert!(stored.get("native_session").is_none());
    let reloaded = fixture.manager().read_record("worker").unwrap();
    assert!(reloaded.session_asserted());
    assert_eq!(reloaded.target().unwrap().session_value, None);
}

#[test]
fn nested_launch_rejects_duplicate_flat_authority() {
    let fixture = Fixture::new();
    let mut nested = started_nested(&fixture);
    nested["harness"] = json!("claude");
    store(&fixture, &nested);
    let error = fixture.manager().read_record("worker").unwrap_err();
    assert!(
        error.to_string().contains("agentctl-session/v2 fields"),
        "{error}"
    );
}

/// One malformed nested row: the words its refusal contains and how the row is changed.
type Mutation = (&'static str, fn(&mut Value));

#[test]
fn nested_launch_rejects_malformed_runtime_authority() {
    let mutations: [Mutation; 6] = [
        ("lifecycle", |row| row["lifecycle"] = json!("banana")),
        ("ownership or adapter", |row| {
            row["launch"]["runtime_ownership"] = json!("foreign")
        }),
        ("launch argv", |row| {
            row["launch"]["argv"] = json!(["claude", "--no-alt-screen"])
        }),
        ("launch contract", |row| {
            row["launch"]["profile"] = json!(17)
        }),
        ("runner identity", |row| {
            row["runner_identity"] = json!({"pid": 1})
        }),
        ("native session identity", |row| {
            row["session_agent"] = json!("claude")
        }),
    ];
    for (expected, mutation) in mutations {
        let fixture = Fixture::new();
        let mut nested = started_nested(&fixture);
        mutation(&mut nested);
        store(&fixture, &nested);

        let error = fixture
            .manager()
            .stop_with_options(
                "worker",
                StopOptions {
                    expected_token: Some(nested["token"].as_str().unwrap().to_owned()),
                    ..StopOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{expected}: {error}");
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }
}

#[test]
fn nested_launch_rejects_nonnull_interactive_permission_mode() {
    let fixture = Fixture::new();
    let mut nested = started_nested(&fixture);
    nested["launch"]["schema"] = json!("agentctl-launch/v2");
    nested["launch"]["permission_mode"] = json!("bypass");
    store(&fixture, &nested);
    let error = fixture.manager().read_record("worker").unwrap_err();
    assert!(error.to_string().contains("launch contract"), "{error}");

    // A v2 launch specification with a null permission mode is an ordinary interactive one.
    nested["launch"]["permission_mode"] = Value::Null;
    store(&fixture, &nested);
    fixture.manager().pause("worker", true).unwrap();
    assert_eq!(stored(&fixture)["launch"], nested["launch"]);
}

#[test]
fn running_nested_muse_without_process_identity_cannot_close_pane() {
    let fixture = Fixture::new();
    fixture
        .manager()
        .start(
            "worker",
            &fixture.root,
            StartOptions {
                harness: "muse".to_owned(),
                ..StartOptions::default()
            },
        )
        .unwrap();
    let mut nested = nested_v2_record(&stored(&fixture));
    assert!(!nested["custom_process_identity"].is_null());
    nested["custom_process_identity"] = Value::Null;
    store(&fixture, &nested);

    let error = fixture
        .manager()
        .stop_with_options(
            "worker",
            StopOptions {
                expected_token: Some(nested["token"].as_str().unwrap().to_owned()),
                ..StopOptions::default()
            },
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("no runtime process identity"),
        "{error}"
    );
    assert!(fixture.client.closed.lock().unwrap().is_empty());
}

#[test]
fn write_back_refuses_a_changed_launch_projection() {
    let fixture = Fixture::new();
    started_nested(&fixture);
    let manager = fixture.manager();
    let mut record = manager.read_record("worker").unwrap();
    record.model = Some("another-model".to_owned());
    let error = manager.save(&record).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("decoded nested launch intent changed; migrate through a canonical writer"),
        "{error}"
    );
    let mut record = manager.read_record("worker").unwrap();
    record.arguments.push("--extra".to_owned());
    assert!(manager.save(&record).is_err());
    assert_eq!(stored(&fixture)["schema"], "agentctl-session/v2");
}

#[test]
fn write_back_keeps_extensions_and_launch_bytes() {
    let fixture = Fixture::new();
    let mut nested = started_nested(&fixture);
    nested["extensions"] = json!({"review": {"labels": ["persistent"]}});
    nested["launch"]["environment_names"] = json!(["REVIEW_MODE"]);
    store(&fixture, &nested);
    fixture.manager().pause("worker", true).unwrap();
    fixture.manager().pause("worker", false).unwrap();
    let stored = stored(&fixture);
    assert_eq!(stored["extensions"], nested["extensions"]);
    assert_eq!(stored["launch"], nested["launch"]);
    assert_eq!(stored, nested);
}

#[test]
fn nested_row_is_listed_in_its_nested_form() {
    let fixture = Fixture::new();
    let nested = started_nested(&fixture);
    let rows = fixture.manager().list().unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert!(row.get("record_error").is_none(), "{row}");
    assert_eq!(row["schema"], "agentctl-session/v2");
    assert_eq!(row["launch"], nested["launch"]);
    assert_eq!(row["agent_status"], "idle");
    assert!(row.get("harness").is_none());
}

#[test]
fn stop_archives_a_nested_row_in_its_nested_form() {
    let fixture = Fixture::new();
    let nested = started_nested(&fixture);
    let stopped = fixture.manager().stop("worker").unwrap();
    let archived = agent::read_private_json(
        &PathBuf::from(stopped["archive"].as_str().unwrap()).join("agent.json"),
    )
    .unwrap();
    let mut expected = nested;
    expected["lifecycle"] = json!("stopped");
    assert_eq!(archived, expected);
}

#[test]
fn rename_refuses_a_nested_row_and_names_anchor() {
    let fixture = Fixture::new();
    started_nested(&fixture);
    let error = fixture.manager().rename("worker", "reviewer").unwrap_err();
    assert_eq!(
        error.to_string(),
        "agent 'worker' uses a nested record format; run `agentctl anchor worker`, which rewrites it as schema 1, then rename"
    );
    assert_eq!(stored(&fixture)["schema"], "agentctl-session/v2");
}

#[test]
fn anchor_migrates_a_v2_row_to_flat_schema_1() {
    let fixture = Fixture::new();
    let nested = started_nested(&fixture);
    let anchored = fixture.manager().anchor("worker", false).unwrap();
    assert_eq!(anchored["migrated_from"], "agentctl-session/v2");
    let flat = stored(&fixture);
    assert_eq!(flat["schema"], 1);
    assert_eq!(flat["harness"], "codex");
    assert_eq!(flat["arguments"], json!(["--no-alt-screen"]));
    assert_eq!(flat["session_value"], nested["session_value"]);
    assert_eq!(flat["anchor_rule"], ANCHOR_RULE);
    assert_eq!(flat["terminal_id"], anchored["terminal_id"]);
    // The runner identity is not a schema-1 field, so it stays as unknown metadata.
    assert_eq!(flat["runner_identity"], Value::Null);
    assert!(flat.get("launch").is_none() && flat.get("extensions").is_none());
    assert_eq!(
        fixture.manager().anchor("worker", false).unwrap()["migrated_from"],
        Value::Null
    );
}

#[test]
fn anchor_keeps_an_asserted_v3_session_asserted() {
    let fixture = Fixture::new();
    fixture.start(None);
    let nested = as_v3(
        nested_v2_record(&stored(&fixture)),
        json!({
            "schema": "agentctl-native-session/v1",
            "agent": "codex",
            "value": "asserted-session",
            "source": "asserted",
        }),
    );
    store(&fixture, &nested);
    fixture
        .client
        .report_session
        .store(false, Ordering::Relaxed);

    let anchored = fixture.manager().anchor("worker", false).unwrap();
    assert_eq!(anchored["migrated_from"], "agentctl-session/v3");
    let flat = stored(&fixture);
    assert_eq!(flat["schema"], 1);
    assert_eq!(flat["goal_session_id"], "asserted-session");
    assert_eq!(flat["session_agent"], Value::Null);
    assert_eq!(flat["session_value"], Value::Null);
    assert_eq!(flat["native_session"]["source"], "asserted");
    let reloaded = fixture.manager().read_record("worker").unwrap();
    assert!(!reloaded.session_asserted());
    assert_eq!(reloaded.target().unwrap().session_value, None);
}

#[test]
fn anchor_failure_leaves_the_nested_row_unchanged() {
    let fixture = Fixture::new();
    let nested = started_nested(&fixture);
    fixture
        .client
        .missing_harness_identity
        .store(true, Ordering::Relaxed);
    assert!(fixture.manager().anchor("worker", false).is_err());
    assert_eq!(stored(&fixture), nested);
}

#[test]
fn unknown_session_schema_is_an_invalid_flat_record() {
    let fixture = Fixture::new();
    let mut nested = started_nested(&fixture);
    nested["schema"] = json!("agentctl-session/v4");
    store(&fixture, &nested);
    let error = fixture.manager().read_record("worker").unwrap_err();
    assert!(
        error.to_string().starts_with("invalid agent record "),
        "{error}"
    );
}

#[test]
fn revive_starts_a_flat_generation_and_archives_the_nested_row() {
    let fixture = Fixture::new();
    fixture
        .client
        .fresh_presentations
        .store(true, Ordering::Relaxed);
    fixture
        .manager()
        .start(
            "worker",
            &fixture.root,
            StartOptions {
                harness: "muse".to_owned(),
                ..StartOptions::default()
            },
        )
        .unwrap();
    let nested = nested_v2_record(&stored(&fixture));
    store(&fixture, &nested);
    let old = fixture.manager().read_record("worker").unwrap();
    fixture
        .client
        .dead_panes
        .lock()
        .unwrap()
        .insert(old.pane_id.clone().unwrap());
    fixture.client.started.store(false, Ordering::Relaxed);
    fixture.client.custom_alive.store(false, Ordering::Relaxed);

    let result = fixture
        .manager()
        .revive("worker", false, Some(&old.token), Duration::from_secs(30))
        .unwrap();
    assert_eq!(result["revived"], true, "{result}");
    let new = stored(&fixture);
    assert_eq!(new["schema"], 1);
    assert_eq!(new["revived_from"], old.token);
    let session = old.session_value.clone().unwrap();
    assert_eq!(new["arguments"], json!(["resume", session]));
    let archive = fixture
        .root
        .join("registry/archive")
        .join(format!("worker-{}", old.token))
        .join("agent.json");
    let mut expected = nested;
    expected["lifecycle"] = json!("stopped");
    assert_eq!(agent::read_private_json(&archive).unwrap(), expected);
}

#[test]
fn a_v2_row_without_a_session_queues_until_anchored_then_drains() {
    // The other edition's test_anchor_migrates_a_nested_record_to_schema_1.
    let fixture = Fixture::new();
    fixture.start(None);
    let mut flat = stored(&fixture);
    for key in ["terminal_id", "harness_identity", "name_history"] {
        flat.as_object_mut().unwrap().remove(key);
    }
    let mut nested = nested_v2_record(&flat);
    nested["session_agent"] = Value::Null;
    nested["session_value"] = Value::Null;
    store(&fixture, &nested);
    *fixture.client.session_override.lock().unwrap() = Some((None, None));

    let error = fixture
        .manager()
        .send("worker", "queued until anchored", DrainOptions::default())
        .unwrap_err();
    assert!(
        error.to_string().contains("agentctl anchor worker"),
        "{error}"
    );
    let anchored = fixture.manager().anchor("worker", false).unwrap();
    assert_eq!(anchored["migrated_from"], "agentctl-session/v2");
    let migrated = stored(&fixture);
    assert_eq!(migrated["schema"], 1);
    assert!(!migrated["harness_identity"].is_null(), "{migrated}");
    fixture
        .manager()
        .drain("worker", DrainOptions::default())
        .unwrap();
    assert_eq!(
        fixture
            .client
            .runs
            .lock()
            .unwrap()
            .last()
            .map(String::as_str),
        Some("queued until anchored")
    );
}
