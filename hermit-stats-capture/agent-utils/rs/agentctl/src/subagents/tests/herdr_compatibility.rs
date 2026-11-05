use super::*;

#[derive(Default)]
struct Clock(Mutex<Duration>);

impl agent::AgentRuntime for Clock {
    fn monotonic(&self) -> Duration {
        *self.0.lock().unwrap()
    }

    fn sleep(&self, duration: Duration) {
        *self.0.lock().unwrap() += duration;
    }
}

pub(super) struct MuseEditor {
    mode: String,
    staged: Option<String>,
    history: Vec<String>,
}

impl Default for MuseEditor {
    fn default() -> Self {
        Self {
            mode: "Auto-review".to_owned(),
            staged: None,
            history: Vec::new(),
        }
    }
}

impl MuseEditor {
    pub(super) fn screen(&self) -> String {
        let editor = self
            .staged
            .as_ref()
            .map_or_else(|| "❯".to_owned(), |text| format!("❯ {text}"));
        format!(
            "Muse Code 1.3.0\n{}\n────────────\n{editor}\n────────────\nwatermelon-preview · xhigh · /work/project · {}\n",
            self.history.join("\n"), self.mode
        )
    }

    pub(super) fn paste(&mut self, prompt: &str) {
        assert!(
            self.staged.is_none(),
            "test input must target an empty editor"
        );
        self.staged = Some(prompt.to_owned());
    }

    pub(super) fn enter(&mut self) -> Option<String> {
        let prompt = self.staged.take()?;
        self.history.push(format!("❯ {prompt}\n◆ response"));
        Some(prompt)
    }
}

fn prepare_muse(fixture: &Fixture, report_native: bool) -> AdoptOptions {
    fixture.prepare_foreign();
    fixture
        .client
        .custom_reported
        .store(true, Ordering::Relaxed);
    fixture
        .client
        .report_session
        .store(report_native, Ordering::Relaxed);
    *fixture.client.muse_editor.lock().unwrap() = Some(MuseEditor::default());
    let mut options = fixture.adopt_options();
    options.harness = "muse".to_owned();
    options
}

fn quick_delivery() -> DrainOptions {
    DrainOptions {
        ready_timeout: Duration::ZERO,
        working_timeout: Duration::from_millis(50),
        ..DrainOptions::default()
    }
}

fn no_input(fixture: &Fixture) {
    assert!(fixture.client.literal_pastes.lock().unwrap().is_empty());
    assert!(fixture.client.keys_sent.lock().unwrap().is_empty());
    assert!(fixture.client.runs.lock().unwrap().is_empty());
    assert!(fixture.client.closed.lock().unwrap().is_empty());
}

#[test]
fn herdr_093_muse_adoption_keeps_foreign_ownership_and_initial_runtime_anchors() {
    for report_native in [false, true] {
        let fixture = Fixture::new();
        let options = prepare_muse(&fixture, report_native);
        fixture
            .client
            .expect_supported
            .store(true, Ordering::Relaxed);
        let manager = fixture.manager();
        let status = manager.adopt("foreign", options).unwrap();
        let record = manager.load("foreign").unwrap();
        assert_eq!(status["adapter"], "herdr-foreign");
        assert_eq!(status["agent_status"], "idle");
        assert_eq!(record.harness, "muse");
        assert_eq!(record.harness_anchor(), Some(&Fake::harness(300)));
        assert_eq!(record.terminal_id.as_deref(), Some("term-owned"));
        assert_eq!(
            record.foreign_shell_identity,
            Some(Fake::foreign_shell_identity())
        );
        assert!(record.custom_process_identity.is_none());
        assert!(!record.pane_reported_by_agentctl);
        assert!(fixture.client.labels.lock().unwrap().is_empty());
        assert!(fixture.client.herdr_names.lock().unwrap().is_empty());
        if report_native {
            assert_eq!(
                record.native_session,
                Some(NativeSession::new("muse", "thread", "observed"))
            );
        } else {
            assert!(record.native_session.is_none());
        }
        let prompt = "literal $(not-a-command) ; 'quoted'\nmentions Auto-review";
        let result = manager
            .send_identified_with_runtime(
                "foreign",
                prompt,
                quick_delivery(),
                "literal",
                &Clock::default(),
            )
            .unwrap();
        assert_eq!(result.delivered, ["literal"]);
        assert_eq!(
            *fixture.client.literal_pastes.lock().unwrap(),
            [(
                "owned".to_owned(),
                format!("{BRACKETED_PASTE_START}{prompt}{BRACKETED_PASTE_END}")
            )]
        );
        assert_eq!(
            *fixture.client.keys_sent.lock().unwrap(),
            [("owned".to_owned(), "Enter".to_owned())]
        );
        assert!(fixture.client.runs.lock().unwrap().is_empty());
        let processed = agent::read_private_json(
            &fixture
                .root
                .join("registry/foreign/queue/processed/literal.json"),
        )
        .unwrap();
        assert_eq!(processed["delivery_state"], "processed");
        assert!(processed["confirmed_at"].is_number());
        assert_eq!(processed["delivery_attempts"], 0);
        manager.rename("foreign", "renamed").unwrap();
        let renamed = manager.load("renamed").unwrap();
        assert_eq!(renamed.token, record.token);
        assert_eq!(renamed.harness_identity, record.harness_identity);
        assert_eq!(
            renamed.foreign_shell_identity,
            record.foreign_shell_identity
        );
        assert_eq!(renamed.adapter, "herdr-foreign");
        assert!(fixture.client.herdr_names.lock().unwrap().is_empty());
        assert!(fixture.client.labels.lock().unwrap().is_empty());
        let stopped = manager.stop("renamed").unwrap();
        assert_eq!(stopped["pane_closed"], false);
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert!(fixture
            .client
            .panes
            .lock()
            .unwrap()
            .iter()
            .any(|pane| pane.pane_id == "owned"));
        assert!(Path::new(stopped["archive"].as_str().unwrap())
            .join("queue/processed/literal.json")
            .is_file());
    }
}

#[test]
fn herdr_093_muse_adoption_requires_process_and_terminal_even_when_native_session_matches() {
    for report_native in [false, true] {
        for failure in ["process", "terminal", "empty_terminal", "replacement"] {
            let fixture = Fixture::new();
            let options = prepare_muse(&fixture, report_native);
            match failure {
                "process" => fixture
                    .client
                    .missing_harness_identity
                    .store(true, Ordering::Relaxed),
                "terminal" => fixture
                    .client
                    .missing_terminal
                    .store(true, Ordering::Relaxed),
                "empty_terminal" => {
                    fixture
                        .client
                        .terminals
                        .lock()
                        .unwrap()
                        .insert("owned".to_owned(), String::new());
                }
                _ => fixture
                    .client
                    .replace_harness_after_pin
                    .store(true, Ordering::Relaxed),
            }
            let error = fixture.manager().adopt("foreign", options).unwrap_err();
            assert!(
                error.to_string().contains("harness") || error.to_string().contains("terminal"),
                "{failure}: {error}"
            );
            assert!(!fixture.root.join("registry/foreign").exists());
            no_input(&fixture);
        }
    }
}

#[test]
fn herdr_093_muse_adoption_refuses_native_contradictions_and_duplicate_owners() {
    for pair in [
        (Some("codex"), Some("thread")),
        (Some("muse"), None),
        (None, Some("thread")),
        (Some("muse"), Some("")),
    ] {
        let fixture = Fixture::new();
        let options = prepare_muse(&fixture, true);
        *fixture.client.session_override.lock().unwrap() =
            Some((pair.0.map(str::to_owned), pair.1.map(str::to_owned)));
        assert!(fixture.manager().adopt("foreign", options).is_err());
        assert!(!fixture.root.join("registry/foreign").exists());
        no_input(&fixture);
    }
    let fixture = Fixture::new();
    let options = prepare_muse(&fixture, true);
    fixture.client.panes.lock().unwrap().push(Pane {
        pane_id: "another".to_owned(),
        tab_id: "another-tab".to_owned(),
        workspace_id: "another-workspace".to_owned(),
    });
    fixture
        .client
        .duplicate_session
        .store(true, Ordering::Relaxed);
    assert!(fixture.manager().adopt("foreign", options).is_err());
    assert!(!fixture.root.join("registry/foreign").exists());
    no_input(&fixture);
}

#[test]
fn herdr_093_muse_adoption_archives_failed_final_generation_without_touching_runtime() {
    for failure in ["harness", "shell", "native"] {
        let fixture = Fixture::new();
        let options = prepare_muse(&fixture, true);
        match failure {
            "harness" => fixture
                .client
                .replace_harness_after_save
                .store(true, Ordering::Relaxed),
            "shell" => fixture
                .client
                .change_foreign_shell_after_save
                .store(true, Ordering::Relaxed),
            _ => fixture
                .client
                .change_session_after_save
                .store(true, Ordering::Relaxed),
        }
        let error = fixture.manager().adopt("foreign", options).unwrap_err();
        assert!(
            error.to_string().contains("was not registered"),
            "{failure}: {error}"
        );
        assert!(!fixture.root.join("registry/foreign").exists());
        let archived = fs::read_dir(fixture.root.join("registry/archive"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let document = agent::read_private_json(&archived.join("agent.json")).unwrap();
        assert_eq!(document["lifecycle"], "adopt_failed");
        assert_eq!(document["harness_identity"], json!(Fake::harness(300)));
        no_input(&fixture);
    }
}

#[test]
fn herdr_093_foreign_muse_current_yolo_mode_refuses_input_even_with_historical_auto_review() {
    let fixture = Fixture::new();
    let options = prepare_muse(&fixture, true);
    let manager = fixture.manager();
    manager.adopt("foreign", options).unwrap();
    {
        let mut editor = fixture.client.muse_editor.lock().unwrap();
        let editor = editor.as_mut().unwrap();
        editor
            .history
            .push("❯ a prior prompt mentions Auto-review\n◆ prior response".to_owned());
        editor.mode = "YOLO".to_owned();
    }
    assert_eq!(manager.status("foreign").unwrap()["agent_status"], "idle");
    let error = manager
        .send_identified_with_runtime(
            "foreign",
            "must remain untouched",
            quick_delivery(),
            "yolo",
            &Clock::default(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("Auto-review"), "{error}");
    assert!(matches!(error, AgentError::PossiblySubmitted(_)));
    no_input(&fixture);
    let retained = agent::read_private_json(&error.undelivered().unwrap().artifact).unwrap();
    assert_eq!(retained["delivery_state"], "inflight");
    assert_eq!(retained["possibly_submitted"], true);
    assert_eq!(retained["delivery_attempts"], 1);
    assert!(manager
        .drain_with_runtime("foreign", quick_delivery(), &Clock::default())
        .unwrap()
        .delivered
        .is_empty());
    no_input(&fixture);
}

#[test]
fn herdr_093_foreign_muse_refuses_changed_saved_anchors_before_input_and_quarantines_effect_races()
{
    for change in [
        "harness",
        "shell",
        "terminal",
        "screen_read",
        "effect_harness",
        "effect_terminal",
    ] {
        let fixture = Fixture::new();
        let options = prepare_muse(&fixture, true);
        fixture
            .client
            .expect_supported
            .store(true, Ordering::Relaxed);
        let manager = fixture.manager();
        manager.adopt("foreign", options).unwrap();
        match change {
            "harness" => {
                fixture
                    .client
                    .harness_pids
                    .lock()
                    .unwrap()
                    .insert("owned".to_owned(), 999);
            }
            "shell" => {
                fixture
                    .client
                    .foreign_shell_identity
                    .lock()
                    .unwrap()
                    .starttime_ticks += 1
            }
            "terminal" => {
                fixture
                    .client
                    .terminals
                    .lock()
                    .unwrap()
                    .insert("owned".to_owned(), "replacement".to_owned());
            }
            "screen_read" => fixture
                .client
                .replace_harness_on_read
                .store(true, Ordering::Relaxed),
            "effect_harness" => fixture
                .client
                .replace_harness_before_effect
                .store(true, Ordering::Relaxed),
            _ => fixture
                .client
                .swap_terminal_before_effect
                .store(true, Ordering::Relaxed),
        }
        let error = manager
            .send_identified_with_runtime(
                "foreign",
                "guarded prompt",
                quick_delivery(),
                "guarded",
                &Clock::default(),
            )
            .unwrap_err();
        assert!(
            fixture.client.keys_sent.lock().unwrap().is_empty(),
            "{change}: {error}"
        );
        assert!(fixture.client.runs.lock().unwrap().is_empty());
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        if change == "effect_harness" {
            assert_eq!(fixture.client.literal_pastes.lock().unwrap().len(), 1);
            assert!(matches!(error, AgentError::PossiblySubmitted(_)));
            let document =
                agent::read_private_json(&error.undelivered().unwrap().artifact).unwrap();
            assert_eq!(document["delivery_state"], "inflight");
            assert_eq!(document["possibly_submitted"], true);
        } else {
            assert!(
                fixture.client.literal_pastes.lock().unwrap().is_empty(),
                "{change}: {error}"
            );
        }
    }
}

#[test]
fn herdr_093_managed_codex_unknown_status_drains_with_saved_recipient_checks() {
    let fixture = Fixture::new();
    fixture.start(None);
    *fixture.client.status.lock().unwrap() = Some("unknown".to_owned());
    *fixture.client.screen.lock().unwrap() =
        Some("Codex\n»\n  model high · ~/project\n".to_owned());
    let manager = fixture.manager();
    assert_eq!(manager.status("worker").unwrap()["agent_status"], "unknown");
    let result = manager
        .send_identified_with_runtime(
            "worker",
            "native confirmed prompt",
            quick_delivery(),
            "unknown",
            &Clock::default(),
        )
        .unwrap();
    assert_eq!(result.delivered, ["unknown"]);
    assert_eq!(
        *fixture.client.runs.lock().unwrap(),
        ["native confirmed prompt"]
    );
    assert!(fixture.client.literal_pastes.lock().unwrap().is_empty());
    let document = agent::read_private_json(
        &fixture
            .root
            .join("registry/worker/queue/processed/unknown.json"),
    )
    .unwrap();
    assert_eq!(document["delivery_state"], "processed");
    assert!(document["confirmed_at"].is_number());
}

#[test]
fn herdr_093_managed_codex_unknown_readiness_cannot_replace_saved_recipient_or_confirm_a_write() {
    for failure in ["pin", "read_pin", "busy", "working_ack"] {
        let fixture = Fixture::new();
        fixture.start(None);
        *fixture.client.status.lock().unwrap() = Some("unknown".to_owned());
        *fixture.client.screen.lock().unwrap() = Some("Codex\n»\n  ? for shortcuts\n".to_owned());
        match failure {
            "pin" => {
                fixture
                    .client
                    .harness_pids
                    .lock()
                    .unwrap()
                    .insert("owned".to_owned(), 999);
            }
            "read_pin" => fixture
                .client
                .replace_harness_on_read
                .store(true, Ordering::Relaxed),
            "busy" => {
                *fixture.client.screen.lock().unwrap() =
                    Some("Codex\nWorking (7s)\n»\n  ? for shortcuts\n".to_owned());
            }
            _ => fixture
                .client
                .fail_first_wait
                .store(true, Ordering::Relaxed),
        }
        let manager = fixture.manager();
        let error = manager
            .send_identified_with_runtime(
                "worker",
                "never duplicate",
                quick_delivery(),
                "guarded",
                &Clock::default(),
            )
            .unwrap_err();
        if failure == "working_ack" {
            assert!(matches!(error, AgentError::PossiblySubmitted(_)));
            assert_eq!(*fixture.client.runs.lock().unwrap(), ["never duplicate"]);
            assert!(manager
                .drain_with_runtime("worker", quick_delivery(), &Clock::default())
                .unwrap()
                .delivered
                .is_empty());
            assert_eq!(fixture.client.runs.lock().unwrap().len(), 1);
        } else {
            assert!(
                fixture.client.runs.lock().unwrap().is_empty(),
                "{failure}: {error}"
            );
            let document = agent::read_private_json(
                &fixture
                    .root
                    .join("registry/worker/queue/inbox/guarded.json"),
            )
            .unwrap();
            assert_eq!(document["delivery_attempts"], 0);
            assert_eq!(document["delivery_state"], "pending");
        }
    }
}
