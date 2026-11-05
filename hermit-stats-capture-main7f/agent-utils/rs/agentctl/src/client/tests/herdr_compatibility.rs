use super::*;

const IDLE_CODEX: &str =
    "Codex\n» \u{1b}[2mAsk Codex to do anything\u{1b}[0m\n  model high · ~/project\n";

fn process_document(pid: u32, executable: &Path) -> Value {
    serde_json::from_str(&process_info_response(pid, executable)).unwrap()
}

fn without_argv(document: &Value, argv: Option<Value>) -> Value {
    let mut document = document.clone();
    let process = document["result"]["process_info"]["foreground_processes"][0]
        .as_object_mut()
        .unwrap();
    process.remove("argv");
    if let Some(argv) = argv {
        process.insert("argv".to_owned(), argv);
    }
    process.insert("name".to_owned(), Value::String("muse".to_owned()));
    process.insert(
        "cmdline".to_owned(),
        Value::String("muse --model claimed".to_owned()),
    );
    document
}

fn configure_codex(herdr: &FakeExecutable, pid: u32, executable: &Path, screen: &str) -> Value {
    let mut process = process_document(pid, executable)["result"]["process_info"].clone();
    process["shell_pid"] = Value::from(1);
    let frame = serde_json::json!({
        "pane": {
            "pane_id": "pane", "workspace_id": "workspace", "cwd": "/work/project",
            "tab_id": "tab", "terminal_id": "terminal", "agent": "codex",
            "agent_status": "unknown", "agent_session": {"agent": "codex", "value": "thread"}
        },
        "process": process,
        "screen": screen,
        "mutate_after_read": null,
        "process_queries": 0,
        "replace_process_after_query": null,
        "startup": null,
        "named": null,
        "agent_get_calls": 0,
        "mutate_on_final_agent_get": null,
        "fail_prelaunch_snapshot": false,
        "starts": 0
    });
    fs::write(herdr.root.join("frame.json"), frame.to_string()).unwrap();
    fs::write(
        herdr.root.join("herdr"),
        r#"#!/usr/bin/python3
import json, pathlib, sys
path = pathlib.Path(__file__).with_name('frame.json')
state = json.loads(path.read_text())
args = sys.argv[1:]
if args[:2] == ['pane', 'process-info']:
    print(json.dumps({'result': {'process_info': state['process']}}))
    state['process_queries'] += 1
    replacement = state['replace_process_after_query']
    if replacement is not None and state['process_queries'] == replacement['after']:
        state['process'] = replacement['process']
elif args[:2] == ['pane', 'get']:
    if state['fail_prelaunch_snapshot'] and state['starts'] == 0:
        raise SystemExit('prelaunch inspection unavailable')
    print(json.dumps({'result': {'pane': state['pane']}}))
elif args[:2] == ['pane', 'read']:
    print(state['screen'], end='')
    change = state['mutate_after_read']
    if change == 'process':
        state['process']['foreground_process_group_id'] += 1
    elif change == 'session':
        state['pane']['agent_session']['value'] = 'replacement-thread'
    elif change in ('terminal_id', 'cwd', 'workspace_id', 'agent'):
        state['pane'][change] = 'replacement'
    state['mutate_after_read'] = None
elif args[:2] == ['agent', 'start']:
    state['starts'] += 1
    state['pane'] = state['launched_pane']
    path.write_text(json.dumps(state))
    print(state['startup']['stdout'], end='')
    print(state['startup']['stderr'], end='', file=sys.stderr)
    raise SystemExit(state['startup']['status'])
elif args[:2] == ['agent', 'get']:
    state['agent_get_calls'] += 1
    if state['agent_get_calls'] == 2 and state['mutate_on_final_agent_get'] is not None:
        state['named'].update(state['mutate_on_final_agent_get'])
    print(json.dumps({'result': {'agent': state['named']}}))
else:
    raise SystemExit('unexpected test command')
path.write_text(json.dumps(state))
"#,
    )
    .unwrap();
    frame
}

fn configure_start(herdr: &FakeExecutable, pid: u32, executable: &Path, screen: &str) -> Value {
    let mut frame = configure_codex(herdr, pid, executable, screen);
    frame["launched_pane"] = frame["pane"].clone();
    frame["pane"]["agent"] = Value::Null;
    frame["pane"]["agent_session"] = Value::Null;
    frame["named"] = serde_json::json!({
        "name": "worker", "pane_id": "pane", "tab_id": "tab", "terminal_id": "terminal"
    });
    frame["startup"] = serde_json::json!({
        "status": 1, "stdout": "",
        "stderr": "{\"id\":\"cli:agent:start\",\"error\":{\"code\":\"timeout\",\"message\":\"timed out waiting for agent startup\"}}"
    });
    fs::write(herdr.root.join("frame.json"), frame.to_string()).unwrap();
    frame
}

fn start_once(herdr: &FakeExecutable, kind: &str, success: bool) {
    let result = herdr
        .client()
        .start_agent("worker", kind, "pane", &[], Duration::from_millis(25));
    assert_eq!(result.is_ok(), success, "{result:?}");
    let frame: Value =
        serde_json::from_slice(&fs::read(herdr.root.join("frame.json")).unwrap()).unwrap();
    assert_eq!(frame["starts"], 1, "startup must never be repeated");
    if !success {
        assert!(result.unwrap_err().to_string().contains("agent start"));
    }
}

#[test]
fn herdr_093_missing_argv_uses_only_coherent_open_image_or_recorded_generation() {
    let herdr = FakeExecutable::new("{}");
    let harness = herdr.root.join("muse");
    install_test_elf("/bin/sleep", &harness);
    let pinned = pin_harness_executable(harness.clone()).unwrap();
    let process = spawn_test_process(&harness);
    let document = process_document(process.0.id(), &harness);
    let identity = live_custom_process(u64::from(process.0.id()))
        .unwrap()
        .identity;
    for argv in [None, Some(Value::Null), Some(serde_json::json!([]))] {
        let frame = without_argv(&document, argv);
        herdr.set_response(&frame.to_string());
        assert_eq!(
            herdr
                .client()
                .custom_harness_observed("pane", &pinned, &|| false)
                .unwrap(),
            Some(identity.clone())
        );
        assert!(herdr
            .client()
            .recorded_custom_harness_observed("pane", &identity, &|| false)
            .unwrap());
        let mut changed = identity.clone();
        changed.starttime_ticks += 1;
        assert!(!herdr
            .client()
            .recorded_custom_harness_observed("pane", &changed, &|| false)
            .unwrap());
    }
    let unrelated = spawn_test_process(Path::new("/bin/sleep"));
    let frame = without_argv(&process_document(unrelated.0.id(), &harness), None);
    herdr.set_response(&frame.to_string());
    assert!(herdr
        .client()
        .custom_harness_observed("pane", &pinned, &|| false)
        .unwrap()
        .is_none());
    assert!(!herdr
        .client()
        .recorded_custom_harness_observed("pane", &identity, &|| false)
        .unwrap());
}

#[test]
fn herdr_093_malformed_argv_is_not_unknown_process_evidence() {
    let executable = fs::canonicalize("/bin/sleep").unwrap();
    let process = spawn_test_process(&executable);
    let document = process_document(process.0.id(), &executable);
    let herdr = FakeExecutable::new("{}");
    let identity = live_custom_process(u64::from(process.0.id()))
        .unwrap()
        .identity;
    for argv in [
        serde_json::json!("claimed command"),
        serde_json::json!(42),
        serde_json::json!([null]),
        serde_json::json!([""]),
        serde_json::json!(["/bin/sleep", 30]),
        serde_json::json!(["/bin/sleep\u{0000}"]),
    ] {
        herdr.set_response(&without_argv(&document, Some(argv)).to_string());
        assert!(herdr
            .client()
            .recorded_custom_harness_observed("pane", &identity, &|| false)
            .is_err());
        assert!(herdr
            .client()
            .verify_harness_identity("pane", &identity)
            .is_err());
    }
}

#[test]
fn herdr_093_missing_argv_does_not_prove_an_idle_shell() {
    let executable = fs::canonicalize("/bin/bash").unwrap();
    let mut command = Command::new(&executable);
    command
        .args(["--noprofile", "--norc"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    let shell = ChildGuard(command.spawn().unwrap());
    let mut document = process_document(shell.0.id(), &executable);
    document["result"]["process_info"]["shell_pid"] = Value::from(shell.0.id());
    let herdr = FakeExecutable::new(&without_argv(&document, None).to_string());
    assert!(herdr.client().pane_shell_identity("pane").is_ok());
    assert!(!herdr.client().pane_is_idle_shell("pane").unwrap());
}

#[test]
fn herdr_093_unknown_codex_ready_requires_a_stable_process_terminal_and_screen() {
    let herdr = FakeExecutable::new("{}");
    let executable = fs::canonicalize("/bin/sleep").unwrap();
    let process = spawn_test_process(&executable);
    configure_codex(&herdr, process.0.id(), &executable, IDLE_CODEX);
    let client = herdr.client();
    let info = client.pane_info("pane").unwrap();
    assert!(client
        .codex_idle_ready_with_cancellation(&info, &|| false)
        .unwrap());
    assert_eq!(client.pane_info("pane").unwrap().status, "unknown");
    for change in [
        "process",
        "session",
        "terminal_id",
        "cwd",
        "workspace_id",
        "agent",
    ] {
        let mut frame = configure_codex(&herdr, process.0.id(), &executable, IDLE_CODEX);
        frame["mutate_after_read"] = Value::from(change);
        fs::write(herdr.root.join("frame.json"), frame.to_string()).unwrap();
        let info = client.pane_info("pane").unwrap();
        assert!(
            !client
                .codex_idle_ready_with_cancellation(&info, &|| false)
                .unwrap(),
            "{change}"
        );
    }
    let mut frame = configure_codex(&herdr, process.0.id(), &executable, IDLE_CODEX);
    frame["process"]["shell_pid"] = Value::from(process.0.id());
    fs::write(herdr.root.join("frame.json"), frame.to_string()).unwrap();
    let info = client.pane_info("pane").unwrap();
    assert!(!client
        .codex_idle_ready_with_cancellation(&info, &|| false)
        .unwrap());
}

#[test]
fn herdr_093_unknown_codex_ready_refuses_drafts_dialogs_unpinned_and_malformed_identities() {
    let herdr = FakeExecutable::new("{}");
    let executable = fs::canonicalize("/bin/sleep").unwrap();
    let process = spawn_test_process(&executable);
    let client = herdr.client();
    for screen in [
        "» operator draft\n  ? for shortcuts\n",
        "Working · esc to interrupt\n»\n  ? for shortcuts\n",
        "Approval required\n»\n  ? for shortcuts\n",
        "Trust this folder?\n›\n  Trust and continue\n",
        "shell$\n",
    ] {
        configure_codex(&herdr, process.0.id(), &executable, screen);
        let info = client.pane_info("pane").unwrap();
        assert!(
            !client
                .codex_idle_ready_with_cancellation(&info, &|| false)
                .unwrap(),
            "{screen:?}"
        );
    }
    for field in [
        "agent",
        "agent_status",
        "terminal_id",
        "provider",
        "value",
        "partial",
    ] {
        let mut frame = configure_codex(&herdr, process.0.id(), &executable, IDLE_CODEX);
        match field {
            "agent" => frame["pane"][field] = Value::from("claude"),
            "agent_status" => frame["pane"][field] = Value::from("blocked"),
            "terminal_id" => frame["pane"][field] = Value::Null,
            "provider" => frame["pane"]["agent_session"]["agent"] = Value::from("claude"),
            "value" => frame["pane"]["agent_session"]["value"] = Value::from(""),
            _ => frame["pane"]["agent_session"]["value"] = Value::Null,
        }
        fs::write(herdr.root.join("frame.json"), frame.to_string()).unwrap();
        let info = client.pane_info("pane").unwrap();
        assert!(
            !client
                .codex_idle_ready_with_cancellation(&info, &|| false)
                .unwrap(),
            "{field}"
        );
    }
}

#[test]
fn herdr_093_start_accepts_only_the_exact_codex_timeout_with_independent_ready_proof() {
    let herdr = FakeExecutable::new("{}");
    let executable = fs::canonicalize("/bin/sleep").unwrap();
    let process = spawn_test_process(&executable);
    configure_start(&herdr, process.0.id(), &executable, IDLE_CODEX);
    start_once(&herdr, "codex", true);
    assert_eq!(herdr.client().pane_info("pane").unwrap().status, "unknown");

    let mut frame = configure_start(&herdr, process.0.id(), &executable, IDLE_CODEX);
    frame["launched_pane"]["agent_session"] = Value::Null;
    fs::write(herdr.root.join("frame.json"), frame.to_string()).unwrap();
    start_once(&herdr, "codex", true);

    configure_start(&herdr, process.0.id(), &executable, IDLE_CODEX);
    start_once(&herdr, "claude", false);
}

#[test]
fn herdr_093_start_never_forgives_near_timeout_envelopes_or_other_failures() {
    let herdr = FakeExecutable::new("{}");
    let executable = fs::canonicalize("/bin/sleep").unwrap();
    let process = spawn_test_process(&executable);
    for (status, stdout, stderr) in [
        (
            2,
            "",
            r#"{"id":"cli:agent:start","error":{"code":"timeout","message":"timed out waiting for agent startup"}}"#,
        ),
        (
            1,
            "{\"result\":{}}",
            r#"{"id":"cli:agent:start","error":{"code":"timeout","message":"timed out waiting for agent startup"}}"#,
        ),
        (1, "", "timed out waiting for agent startup"),
        (
            1,
            "",
            r#"{"id":"cli:agent:start","error":{"code":"agent_not_ready","message":"timed out waiting for agent startup"}}"#,
        ),
        (
            1,
            "",
            r#"{"id":"cli:agent:start:timeout","error":{"code":"timeout","message":"timed out waiting for agent startup"}}"#,
        ),
        (
            1,
            "",
            r#"{"id":"cli:agent:start","error":{"code":"timeout","message":"timeout while trusting folder"}}"#,
        ),
        (
            1,
            "",
            r#"{"id":"cli:agent:start","extra":true,"error":{"code":"timeout","message":"timed out waiting for agent startup"}}"#,
        ),
        (
            1,
            "",
            r#"{"id":"cli:agent:start","error":{"code":"timeout","message":"timed out waiting for agent startup","extra":true}}"#,
        ),
        (
            1,
            "",
            r#"{"id":"other","id":"cli:agent:start","error":{"code":"timeout","message":"timed out waiting for agent startup"}}"#,
        ),
        (
            1,
            "",
            r#"{"id":"cli:agent:start","error":{"code":"other","code":"timeout","message":"timed out waiting for agent startup"}}"#,
        ),
        (
            1,
            "",
            r#"{"id":"cli:agent:start","error":{"code":"timeout","message":"other","message":"timed out waiting for agent startup"}}"#,
        ),
        (1, "", "{broken"),
    ] {
        let mut frame = configure_start(&herdr, process.0.id(), &executable, IDLE_CODEX);
        frame["startup"] =
            serde_json::json!({"status": status, "stdout": stdout, "stderr": stderr});
        fs::write(herdr.root.join("frame.json"), frame.to_string()).unwrap();
        start_once(&herdr, "codex", false);
        let frame: Value =
            serde_json::from_slice(&fs::read(herdr.root.join("frame.json")).unwrap()).unwrap();
        assert_eq!(frame["process_queries"], 0, "{stderr}");
    }
}

#[test]
fn herdr_093_start_refuses_busy_dialog_draft_and_changed_presentation_or_name() {
    let herdr = FakeExecutable::new("{}");
    let executable = fs::canonicalize("/bin/sleep").unwrap();
    let process = spawn_test_process(&executable);
    for screen in [
        "Codex\nWorking (7s)\n»\n  ? for shortcuts\n",
        "Codex\nExploring (1m 2s • Ctrl+c to interrupt)\n»\n  ? for shortcuts\n",
        "Codex\nApproval required\n»\n  ? for shortcuts\n",
        "Codex\n» operator draft\n  ? for shortcuts\n",
        "Trust this folder?\n›\n  Trust and continue\n",
    ] {
        configure_start(&herdr, process.0.id(), &executable, screen);
        start_once(&herdr, "codex", false);
    }
    for field in [
        "agent",
        "agent_status",
        "terminal_id",
        "workspace_id",
        "cwd",
        "tab_id",
        "name",
        "named_pane",
        "final_name",
        "initial_agent",
        "missing_initial",
    ] {
        let mut frame = configure_start(&herdr, process.0.id(), &executable, IDLE_CODEX);
        match field {
            "name" => frame["named"]["name"] = Value::from("another"),
            "named_pane" => frame["named"]["pane_id"] = Value::from("another"),
            "final_name" => {
                frame["mutate_on_final_agent_get"] = serde_json::json!({"pane_id": "another"})
            }
            "initial_agent" => frame["pane"]["agent"] = Value::from("codex"),
            "missing_initial" => frame["fail_prelaunch_snapshot"] = Value::from(true),
            "agent_status" => frame["launched_pane"][field] = Value::from("blocked"),
            _ => frame["launched_pane"][field] = Value::from("another"),
        }
        fs::write(herdr.root.join("frame.json"), frame.to_string()).unwrap();
        start_once(&herdr, "codex", false);
    }
}

#[test]
fn herdr_093_start_pins_the_original_process_across_each_readiness_observation() {
    let herdr = FakeExecutable::new("{}");
    let executable = fs::canonicalize("/bin/sleep").unwrap();
    let original = spawn_test_process(&executable);
    let replacement = spawn_test_process(&executable);
    let mut frame = configure_start(&herdr, original.0.id(), &executable, IDLE_CODEX);
    let mut replacement_info =
        process_document(replacement.0.id(), &executable)["result"]["process_info"].clone();
    replacement_info["shell_pid"] = Value::from(1);
    frame["replace_process_after_query"] =
        serde_json::json!({"after": 1, "process": replacement_info});
    fs::write(herdr.root.join("frame.json"), frame.to_string()).unwrap();
    start_once(&herdr, "codex", false);
    for change in ["process", "session", "terminal_id", "workspace_id", "cwd"] {
        let mut frame = configure_start(&herdr, original.0.id(), &executable, IDLE_CODEX);
        frame["mutate_after_read"] = Value::from(change);
        fs::write(herdr.root.join("frame.json"), frame.to_string()).unwrap();
        start_once(&herdr, "codex", false);
    }
}

#[test]
fn herdr_093_native_start_success_keeps_its_existing_barrier_when_fallback_is_unavailable() {
    let herdr = FakeExecutable::new("{}");
    let executable = fs::canonicalize("/bin/sleep").unwrap();
    let process = spawn_test_process(&executable);
    for kind in ["codex", "claude"] {
        let mut frame = configure_start(&herdr, process.0.id(), &executable, "not a composer");
        frame["fail_prelaunch_snapshot"] = Value::from(true);
        frame["startup"] =
            serde_json::json!({"status": 0, "stdout": "{\"result\":{}}", "stderr": ""});
        fs::write(herdr.root.join("frame.json"), frame.to_string()).unwrap();
        start_once(&herdr, kind, true);
        let frame: Value =
            serde_json::from_slice(&fs::read(herdr.root.join("frame.json")).unwrap()).unwrap();
        assert_eq!(frame["process_queries"], 0);
        assert_eq!(frame["agent_get_calls"], 0);
    }
}

#[test]
fn herdr_093_muse_uses_only_the_active_mode_and_complete_literal_editor() {
    let frame = |editor: &str, footer: &str| {
        format!("Muse Code 1.3.0\nprior answer mentioned Auto-review\n────────────\n{editor}\n────────────\n{footer}\n")
    };
    let auto_review = "watermelon-preview · xhigh · /work/project · Auto-review";
    let yolo = "watermelon-preview · xhigh · /work/project · YOLO";
    assert!(muse_idle_composer(&frame("❯", auto_review)));
    assert!(muse_auto_review_idle_composer(&frame("❯", auto_review)));
    assert!(muse_idle_composer(&frame("❯", yolo)));
    assert!(!muse_auto_review_idle_composer(&frame("❯", yolo)));
    let cropped = frame("❯", yolo).replace("Muse Code 1.3.0\n", "");
    assert!(!muse_idle_composer(&cropped));
    assert!(muse_verified_process_idle_composer(&cropped));
    for footer in [
        "unrecognized Auto-review",
        "watermelon-preview · xhigh · /work/project · unknown",
    ] {
        assert!(!muse_auto_review_idle_composer(&frame("❯", footer)));
    }
    assert!(!muse_auto_review_idle_composer(&frame(
        "❯",
        &format!("{auto_review}\n{yolo}")
    )));
    assert!(!muse_auto_review_idle_composer(&frame(
        "❯ operator draft",
        auto_review
    )));
    let prompt = "literal $(not-a-command) ; 'quoted'\nsecond line";
    assert!(muse_prompt_is_exact_composer(
        &frame(&format!("❯ {prompt}"), auto_review),
        prompt
    ));
    assert!(!muse_prompt_is_exact_composer(
        &frame(&format!("❯ other draft {prompt}"), auto_review),
        prompt
    ));
    assert!(!muse_prompt_is_exact_composer(
        &frame(&format!("❯ {prompt} appended draft"), auto_review),
        prompt
    ));
}
