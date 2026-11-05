use super::*;

fn loaded_resume_profile(
    fixture: &Fixture,
    harness: &str,
    arguments: &[&str],
) -> crate::profiles::LaunchProfile {
    assert!(std::process::Command::new("/usr/bin/git")
        .args(["init", "-q"])
        .arg(&fixture.root)
        .status()
        .unwrap()
        .success());
    fs::write(fixture.root.join(".gitignore"), ".agentctl/\n").unwrap();
    let directory = fixture.root.join(".agentctl");
    agent::create_private_directory(&directory, "test launch profiles", false, false).unwrap();
    agent::atomic_json(
        &directory.join("profiles.json"),
        &json!({
            "schema": "agentctl-profiles/v1",
            "profiles": {"reviewer": {
                "harness": harness,
                "mode": "interactive",
                "model": "profile-model",
                "reasoning_effort": "high",
                "argv": arguments,
                "env": {"REVIEW_MODE": "literal $(setting)"},
            }},
        }),
    )
    .unwrap();
    let (_, mut profiles) = crate::profiles::load_profiles(&fixture.root, false).unwrap();
    profiles.remove("reviewer").unwrap()
}

fn resume_profile_options(profile: &crate::profiles::LaunchProfile, resume: &str) -> StartOptions {
    StartOptions {
        profile: Some(profile.name.clone()),
        harness: profile.harness.clone(),
        model: profile.model.clone(),
        resume: Some(resume.to_owned()),
        harness_args: profile.argv.clone(),
        environment: profile.environment.clone(),
        ..StartOptions::default()
    }
}

#[test]
fn profile_resume_preserves_loaded_policy_and_literal_conversation_for_each_local_harness() {
    let conversation = "saved conversation 'quoted' $(literal)";
    let policy = "--literal-policy=owner choice";
    for harness in ["claude", "codex", "muse"] {
        let fixture = Fixture::new();
        let profile = loaded_resume_profile(&fixture, harness, &[policy]);
        let mut options = resume_profile_options(&profile, conversation);
        options.slot = Some(SlotLaunch {
            slot: "slot-a".to_owned(),
            project: Some(fixture.root.clone()),
            isolation: Some("cgroup".to_owned()),
            executable: Some(fake_wrkslots(&fixture.root, &fixture.root, "ok")),
            ..SlotLaunch::default()
        });
        let status = fixture
            .manager()
            .start_with_reasoning_effort(
                "worker",
                &fixture.root,
                profile.reasoning_effort.as_deref().unwrap(),
                options,
            )
            .unwrap();
        let expected = match harness {
            "claude" => vec![
                "--resume",
                conversation,
                "--model",
                "profile-model",
                "--effort",
                "high",
                policy,
            ],
            "codex" => vec![
                "resume",
                conversation,
                "--no-alt-screen",
                "--model",
                "profile-model",
                "--config",
                "model_reasoning_effort=high",
                policy,
            ],
            "muse" => vec![
                "--model",
                "profile-model",
                "--reasoning-effort",
                "high",
                policy,
                "resume",
                conversation,
            ],
            _ => unreachable!(),
        };
        assert_eq!(status["arguments"], json!(expected));
        let record = fixture.manager().load("worker").unwrap();
        assert_eq!(
            fixture.client.launches.lock().unwrap()["owned"].1,
            record.arguments
        );
        assert_eq!(
            record
                .arguments
                .iter()
                .filter(|argument| argument.as_str() == conversation)
                .count(),
            1
        );
        assert!(!record
            .arguments
            .iter()
            .any(|argument| argument == "--session-id"));
        assert_eq!(status["profile"], "reviewer");
        assert_eq!(status["model"], "profile-model");
        assert_eq!(status["reasoning_effort"], "high");
        assert_eq!(status["resume"], conversation);
        assert_eq!(status["native_session"]["agent"], harness);
        assert_eq!(status["native_session"]["value"], conversation);
        assert_eq!(status["native_session"]["source"], "asserted");
        assert_eq!(status["session_agent"], harness);
        assert_eq!(status["session_value"], conversation);
        let cwd = fs::canonicalize(&fixture.root)
            .unwrap()
            .display()
            .to_string();
        assert_eq!(status["cwd"], cwd);
        assert_eq!(status["slot"], "slot-a");
        assert_eq!(status["slot_project"], cwd);
        assert_eq!(status["slot_isolation"], "cgroup");
        assert_eq!(status["environment_names"], json!(["REVIEW_MODE"]));
        assert!(!status.to_string().contains("literal $(setting)"));
        assert_eq!(fixture.client.slot_commands.lock().unwrap().len(), 1);
        assert_eq!(
            *fixture.client.environments.lock().unwrap(),
            [profile.environment]
        );
    }
}

#[test]
fn profile_resume_wrong_native_reports_refuse_without_submitting_the_brief() {
    for harness in ["claude", "codex", "muse"] {
        let other_provider = if harness == "codex" {
            "claude"
        } else {
            "codex"
        };
        for reported in [(harness, "different-id"), (other_provider, "requested-id")] {
            let fixture = Fixture::new();
            let profile = loaded_resume_profile(&fixture, harness, &[]);
            *fixture.client.session_override.lock().unwrap() =
                Some((Some(reported.0.to_owned()), Some(reported.1.to_owned())));
            let mut options = resume_profile_options(&profile, "requested-id");
            options.brief = Some("never submit this task".to_owned());
            let error = fixture
                .manager()
                .start_with_reasoning_effort(
                    "worker",
                    &fixture.root,
                    profile.reasoning_effort.as_deref().unwrap(),
                    options,
                )
                .unwrap_err();
            assert!(
                error.to_string().contains("different native"),
                "{harness}: {error}"
            );
            assert!(fixture.client.runs.lock().unwrap().is_empty());
            assert!(fixture.client.keys_sent.lock().unwrap().is_empty());
            let record = fixture.manager().load("worker").unwrap();
            assert_eq!(record.lifecycle, "launch_failed");
            assert_eq!(record.profile, Some(profile.name));
            assert_eq!(record.native_session.unwrap().value, "requested-id");
            assert!(record.session_agent.is_none() && record.session_value.is_none());
        }
    }
}

#[test]
fn profile_resume_duplicate_raw_selectors_refuse_before_allocation_without_changing_raw_only_policy(
) {
    for harness in ["claude", "codex", "muse"] {
        let fixture = Fixture::new();
        let arguments = if harness == "claude" {
            vec!["--resume", "raw-conversation"]
        } else {
            vec!["resume", "raw-conversation"]
        };
        let profile = loaded_resume_profile(&fixture, harness, &arguments);
        let error = fixture
            .manager()
            .start_with_reasoning_effort(
                "worker",
                &fixture.root,
                profile.reasoning_effort.as_deref().unwrap(),
                resume_profile_options(&profile, "structured-conversation"),
            )
            .unwrap_err();
        if harness == "claude" {
            assert_eq!(error.to_string(), "Claude conversation selectors must use --resume; agentctl assigns --session-id for new conversations");
        } else {
            assert!(error
                .to_string()
                .contains("cannot repeat the structured resume selector"));
        }
        assert!(!fixture.root.join("registry").exists());
        assert!(fixture.client.environments.lock().unwrap().is_empty());
        assert!(fixture.client.launches.lock().unwrap().is_empty());
        if harness != "claude" {
            let mut options = resume_profile_options(&profile, "unused");
            options.resume = None;
            let status = fixture
                .manager()
                .start_with_reasoning_effort(
                    "worker",
                    &fixture.root,
                    profile.reasoning_effort.as_deref().unwrap(),
                    options,
                )
                .unwrap();
            assert_eq!(status["resume"], Value::Null);
            assert_eq!(status["native_session"]["value"], "raw-conversation");
            assert_eq!(status["native_session"]["source"], "observed");
        }
    }
}

#[test]
fn profile_resume_rejects_an_occupied_asserted_conversation_before_opening_another_tab() {
    for harness in ["claude", "codex", "muse"] {
        let fixture = Fixture::new();
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        let profile = loaded_resume_profile(&fixture, harness, &[]);
        fixture
            .manager()
            .start_with_reasoning_effort(
                "owner",
                &fixture.root,
                profile.reasoning_effort.as_deref().unwrap(),
                resume_profile_options(&profile, "already-bound-session"),
            )
            .unwrap();
        let error = fixture
            .manager()
            .start_with_reasoning_effort(
                "worker",
                &fixture.root,
                profile.reasoning_effort.as_deref().unwrap(),
                resume_profile_options(&profile, "already-bound-session"),
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("already registered as \"owner\""));
        assert!(!fixture.root.join("registry/worker").exists());
        assert_eq!(fixture.client.environments.lock().unwrap().len(), 1);
    }
}

#[test]
fn fresh_claude_records_an_assigned_conversation_and_safe_launch_metadata() {
    let fixture = Fixture::new();
    fixture
        .client
        .report_session
        .store(false, Ordering::Relaxed);
    let status = fixture
        .manager()
        .start_with_reasoning_effort(
            "worker",
            &fixture.root,
            "high",
            StartOptions {
                workspace_id: Some("workspace".to_owned()),
                profile: Some("claude-reviewer".to_owned()),
                harness: "claude".to_owned(),
                model: Some("opus".to_owned()),
                environment: vec![
                    "REVIEW_MODE=private-value".to_owned(),
                    "REVIEW_MODE=another-private-value".to_owned(),
                ],
                ..StartOptions::default()
            },
        )
        .unwrap();
    let native = &status["native_session"];
    assert_eq!(native["schema"], "agentctl-native-session/v1");
    assert_eq!(native["agent"], "claude");
    assert_eq!(native["source"], "asserted");
    let conversation = native["value"].as_str().unwrap();
    assert!(regex::Regex::new(
        "^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$"
    )
    .unwrap()
    .is_match(conversation));
    let record = fixture.manager().load("worker").unwrap();
    assert_eq!(&record.arguments[..2], &["--session-id", conversation]);
    assert!(record
        .arguments
        .windows(2)
        .any(|pair| { pair[0] == "--session-id" && pair[1] == conversation }));
    assert_eq!(
        fixture.client.launches.lock().unwrap()["owned"].1,
        record.arguments
    );
    assert_eq!(status["session_agent"], Value::Null);
    assert_eq!(status["session_value"], Value::Null);
    assert_eq!(status["profile"], "claude-reviewer");
    assert_eq!(status["model"], "opus");
    assert_eq!(status["reasoning_effort"], "high");
    assert_eq!(status["cwd"], fixture.root.display().to_string());
    assert_eq!(status["environment_names"], json!(["REVIEW_MODE"]));
    assert!(!status.to_string().contains("private-value"));
    assert_eq!(
        fixture.manager().list().unwrap()[0]["native_session"],
        *native
    );
}

#[test]
fn fresh_claude_accepts_only_the_assigned_observed_conversation() {
    let fixture = Fixture::new();
    let status = fixture
        .manager()
        .start(
            "worker",
            &fixture.root,
            StartOptions {
                harness: "claude".to_owned(),
                ..StartOptions::default()
            },
        )
        .unwrap();
    assert_eq!(status["native_session"]["value"], status["session_value"]);
    assert_eq!(status["native_session"]["source"], "asserted");
    assert_eq!(status["session_agent"], "claude");
}

#[test]
fn resume_preserves_the_requested_conversation_without_assigning_a_new_one() {
    for harness in ["claude", "codex"] {
        let fixture = Fixture::new();
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        let status = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    harness: harness.to_owned(),
                    resume: Some("requested-conversation".to_owned()),
                    ..StartOptions::default()
                },
            )
            .unwrap();
        assert_eq!(status["native_session"]["value"], "requested-conversation");
        assert_eq!(status["native_session"]["source"], "asserted");
        let arguments = status["arguments"].as_array().unwrap();
        assert!(!arguments.iter().any(|argument| argument == "--session-id"));
        assert!(arguments
            .iter()
            .any(|argument| argument == "requested-conversation"));
    }
}

#[test]
fn a_different_reported_resume_id_or_provider_refuses_before_the_brief() {
    for reported in [("codex", "different-id"), ("claude", "requested-id")] {
        let fixture = Fixture::new();
        *fixture.client.session_override.lock().unwrap() =
            Some((Some(reported.0.to_owned()), Some(reported.1.to_owned())));
        let error = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    resume: Some("requested-id".to_owned()),
                    brief: Some("must not be sent".to_owned()),
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("different native"), "{error}");
        assert!(fixture.client.runs.lock().unwrap().is_empty());
        let record = fixture.manager().load("worker").unwrap();
        assert_eq!(record.lifecycle, "launch_failed");
        assert_eq!(record.native_session.unwrap().value, "requested-id");
        assert!(record.session_agent.is_none() && record.session_value.is_none());
    }
}

#[test]
fn malformed_reported_sessions_leave_a_loadable_failed_record_without_a_brief() {
    for reported in [
        (None, Some("thread")),
        (Some("codex"), None),
        (Some(""), Some("thread")),
        (Some("codex\0"), Some("thread")),
        (Some("codex"), Some("")),
        (Some("codex"), Some("bad\0value")),
        (Some("claude"), None),
    ] {
        let fixture = Fixture::new();
        *fixture.client.session_override.lock().unwrap() =
            Some((reported.0.map(str::to_owned), reported.1.map(str::to_owned)));
        fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    brief: Some("never send this".to_owned()),
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        let record = fixture.manager().load("worker").unwrap();
        assert_eq!(record.lifecycle, "launch_failed");
        assert!(record.session_agent.is_none() && record.session_value.is_none());
        assert!(record.native_session.is_none());
        assert!(fixture.client.runs.lock().unwrap().is_empty());
    }
}

#[test]
fn empty_or_nul_resume_ids_refuse_before_registry_or_runtime_effects() {
    for harness in ["codex", "claude", "muse"] {
        for resume in ["", "bad\0conversation"] {
            let fixture = Fixture::new();
            let error = fixture
                .manager()
                .start(
                    "worker",
                    &fixture.root,
                    StartOptions {
                        harness: harness.to_owned(),
                        resume: Some(resume.to_owned()),
                        ..StartOptions::default()
                    },
                )
                .unwrap_err();
            assert!(error.to_string().contains("native session id"));
            assert!(!fixture.root.join("registry").exists());
            assert!(!fixture.client.started.load(Ordering::Relaxed));
        }
    }
}

#[test]
fn raw_claude_identity_selectors_refuse_before_allocation() {
    for argument in [
        "--session-id=owner-id",
        "--session-id",
        "--resume=other-id",
        "-rother-id",
        "--continue",
        "-c",
        "--fork-session",
    ] {
        let fixture = Fixture::new();
        let error = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    harness: "claude".to_owned(),
                    harness_args: vec![argument.to_owned()],
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Claude conversation selectors must use --resume; agentctl assigns --session-id for new conversations"
        );
        assert!(!fixture.root.join("registry").exists());
        assert!(!fixture.client.started.load(Ordering::Relaxed));
    }
}

#[test]
fn codex_and_muse_capture_reported_sessions_without_inventing_one() {
    for harness in ["codex", "muse"] {
        for reported in [false, true] {
            let fixture = Fixture::new();
            fixture
                .client
                .report_session
                .store(reported, Ordering::Relaxed);
            let status = fixture
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
            if reported {
                assert_eq!(status["native_session"]["source"], "observed");
                assert_eq!(status["native_session"]["agent"], harness);
                assert_eq!(status["native_session"]["value"], status["session_value"]);
            } else {
                assert_eq!(status["native_session"], Value::Null);
            }
        }
    }
}

#[test]
fn an_asserted_conversation_never_substitutes_for_a_routing_anchor() {
    let fixture = Fixture::new();
    fixture
        .client
        .report_session
        .store(false, Ordering::Relaxed);
    fixture
        .manager()
        .start(
            "worker",
            &fixture.root,
            StartOptions {
                harness: "claude".to_owned(),
                ..StartOptions::default()
            },
        )
        .unwrap();
    let manager = fixture.manager();
    let mut record = manager.load("worker").unwrap();
    record.harness_identity = None;
    record.anchor_rule = None;
    manager.save(&record).unwrap();
    let error = manager
        .send("worker", "never send this", DrainOptions::default())
        .unwrap_err();
    assert!(error.to_string().contains("anchor"));
    assert!(fixture.client.runs.lock().unwrap().is_empty());
}

#[test]
fn old_records_default_recovery_fields_and_preserve_unknown_metadata() {
    let fixture = Fixture::new();
    fixture.start(None);
    let path = fixture.root.join("registry/worker/agent.json");
    let mut document = agent::read_private_json(&path).unwrap();
    for key in [
        "native_session",
        "profile",
        "slot",
        "slot_project",
        "slot_isolation",
        "reasoning_effort",
        "environment_names",
    ] {
        document.as_object_mut().unwrap().remove(key);
    }
    document["future_metadata"] = json!({"kept": true});
    agent::atomic_json(&path, &document).unwrap();
    let manager = fixture.manager();
    let record = manager.load("worker").unwrap();
    assert!(record.native_session.is_none() && record.profile.is_none());
    assert!(record.slot.is_none() && record.environment_names.is_empty());
    manager.save(&record).unwrap();
    assert_eq!(
        manager.get("worker").unwrap()["future_metadata"],
        json!({"kept": true})
    );
}

#[test]
fn malformed_or_conflicting_recovery_metadata_is_rejected() {
    let fixture = Fixture::new();
    fixture.start(None);
    let path = fixture.root.join("registry/worker/agent.json");
    let original = agent::read_private_json(&path).unwrap();
    let malformed = [
        (
            "native_session",
            json!({"schema": "agentctl-native-session/v1", "agent": "codex", "value": "thread", "source": "guessed"}),
        ),
        (
            "native_session",
            json!({"schema": "agentctl-native-session/v1", "agent": "claude", "value": "thread", "source": "observed"}),
        ),
        (
            "native_session",
            json!({"schema": "agentctl-native-session/v1", "agent": "codex", "value": "another-thread", "source": "asserted"}),
        ),
        (
            "native_session",
            json!({"schema": "agentctl-native-session/v1", "agent": "codex", "value": "thread", "source": "observed", "unexpected": true}),
        ),
        (
            "native_session",
            json!({"schema": "agentctl-native-session/v1", "agent": "codex", "value": "", "source": "observed"}),
        ),
        ("profile", json!("not a profile")),
        ("slot", json!("bad\0slot")),
        ("slot_project", json!("relative/project")),
        ("slot_isolation", json!("unconfined")),
        ("reasoning_effort", json!("unrecognised")),
        ("environment_names", json!(["TOKEN=private-value"])),
        ("environment_names", json!(["NAME", "NAME"])),
    ];
    for (key, value) in malformed {
        let mut document = original.clone();
        document[key] = value;
        agent::atomic_json(&path, &document).unwrap();
        assert!(fixture.manager().load("worker").is_err(), "accepted {key}");
    }
    let mut document = original;
    document["resume"] = json!("conflicting-request");
    agent::atomic_json(&path, &document).unwrap();
    assert!(fixture.manager().load("worker").is_err());
}

#[test]
fn asserted_conversations_participate_in_duplicate_session_checks() {
    let fixture = Fixture::new();
    fixture
        .client
        .report_session
        .store(false, Ordering::Relaxed);
    let manager = fixture.manager();
    manager
        .start(
            "worker",
            &fixture.root,
            StartOptions {
                resume: Some("shared-conversation".to_owned()),
                ..StartOptions::default()
            },
        )
        .unwrap();
    let error = manager
        .start(
            "second",
            &fixture.root,
            StartOptions {
                resume: Some("shared-conversation".to_owned()),
                ..StartOptions::default()
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("already registered"));
    assert!(!fixture.root.join("registry/second").exists());
}

#[test]
fn bind_session_cannot_replace_recovery_or_legacy_resume_provenance() {
    for legacy in [false, true] {
        let fixture = Fixture::new();
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        let manager = fixture.manager();
        manager
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    resume: Some("original-conversation".to_owned()),
                    ..StartOptions::default()
                },
            )
            .unwrap();
        if legacy {
            let mut record = manager.load("worker").unwrap();
            record.native_session = None;
            manager.save(&record).unwrap();
        }
        let error = manager
            .bind_session("worker", "different-conversation", None)
            .unwrap_err();
        assert!(error.to_string().contains("refusing to replace"));
        assert!(manager.load("worker").unwrap().goal_session_id.is_none());
        manager
            .bind_session("worker", "original-conversation", None)
            .unwrap();
        assert_eq!(
            manager
                .load("worker")
                .unwrap()
                .native_session
                .unwrap()
                .value,
            "original-conversation"
        );
    }
}

#[test]
fn adoption_refuses_a_session_held_only_as_recovery_provenance() {
    let fixture = Fixture::new();
    fixture
        .client
        .report_session
        .store(false, Ordering::Relaxed);
    let manager = fixture.manager();
    manager
        .start(
            "worker",
            &fixture.root,
            StartOptions {
                resume: Some("thread".to_owned()),
                ..StartOptions::default()
            },
        )
        .unwrap();
    let mut reported = Fake::pane("reported");
    reported.tab_id = "reported-tab".to_owned();
    fixture.client.panes.lock().unwrap().push(reported);
    let mut options = fixture.adopt_options();
    options.pane_id = "reported".to_owned();
    let error = manager.adopt("other", options).unwrap_err();
    assert!(error.to_string().contains("already registered"));
    assert!(!fixture.root.join("registry/other").exists());
}
