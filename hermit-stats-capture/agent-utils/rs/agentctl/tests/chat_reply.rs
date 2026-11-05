//! `chat reply` stores a reply of an open request and reports what happened through its exit
//! status: 0 when the request holds the reply, with one JSON object on standard output; 1 when the
//! reply is refused, or when the state cannot be read or written; 2 for a usage error, which
//! includes a reply file that cannot be used; and 75 when nothing was stored but the same command
//! can succeed later.
//!
//! This drives the real binary as a subprocess, so it checks what the process prints and the
//! status it exits with. Each run uses a scratch bridge state that holds one request, a scratch
//! registry, scratch home directories, and a Herdr executable that does not exist, since the
//! command must never need Herdr.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentctl::chat_runtime::{BridgeConfiguration, BridgeState};
use chat_subscription::{
    ChannelId, CommittableEvent, DeliveryBatch, DeliveryId, EventSequence, InboundMessage,
    MessageId, ProviderCursor, SenderId, ThreadId,
};
use serde_json::{json, Value};

/// How long one run may take before it is killed. Each run ends at once, so this bounds a hang.
const RUN_LIMIT: Duration = Duration::from_secs(60);

/// A scratch directory holding a bridge state with one open request from the owner, which no
/// service runs on.
struct Scratch {
    root: PathBuf,
    state: PathBuf,
    key: String,
    reply_id: String,
}

/// The exit code and output of one run.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Scratch {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "agentctl-chat-reply-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home")).expect("create scratch directory");
        let state = root.join("state");
        fs::create_dir(&state).expect("create state directory");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("private state");
        let bridge = BridgeState::initialize(
            &state,
            BridgeConfiguration {
                subscription_plugin: "fixture".to_owned(),
                subscription_environment: Vec::new(),
                channel_ids: vec!["spaces/example".to_owned()],
                allowed_senders: vec!["users/owner".to_owned()],
                agent_name: "worker".to_owned(),
                agent_label: "worker".to_owned(),
                outbound_enabled: true,
                ack_reaction: None,
                backend_configuration: None,
                outbound_command: None,
            },
        )
        .expect("initialize state");
        let message = InboundMessage::new(
            ChannelId::new("spaces/example").expect("channel"),
            MessageId::new("spaces/example/messages/one").expect("message"),
            ThreadId::new("spaces/example/threads/one").expect("thread"),
            SenderId::new("users/owner").expect("sender"),
            "request",
            "2026-09-30T12:00:00Z",
            false,
        )
        .expect("inbound message");
        let batch = DeliveryBatch::new(
            EventSequence::new(1).expect("sequence"),
            ProviderCursor::new("cursor").expect("cursor"),
            DeliveryId::new("delivery").expect("delivery"),
            vec![CommittableEvent::message_created(message)],
        )
        .expect("delivery batch");
        let key = bridge
            .admit_batch(&batch)
            .expect("admit request")
            .new_request_keys
            .remove(0);
        let reply_id = bridge
            .next_reply_route(&key)
            .expect("reply route")
            .expect("open request")
            .identifier;
        Self {
            root,
            state,
            key,
            reply_id,
        }
    }

    /// Write `contents` to the file `name` in the scratch directory, and return its path.
    fn file(&self, name: &str, contents: &[u8]) -> String {
        let path = self.root.join(name);
        fs::write(&path, contents).expect("write reply file");
        path.to_str().expect("UTF-8 path").to_owned()
    }

    /// Run `agentctl chat reply --bridge-state <state>` followed by `arguments`.
    fn reply(&self, arguments: &[&str]) -> Run {
        let home = self.root.join("home");
        let registry = self.root.join("registry");
        let stdout_path = self.root.join("stdout");
        let stderr_path = self.root.join("stderr");
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentctl"))
            .current_dir(&self.root)
            .env("HOME", &home)
            .env("AGENTCTL_HOME", home.join(".agentctl"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_RUNTIME_DIR", home.join("runtime"))
            .arg("--registry")
            .arg(&registry)
            .arg("--herdr-bin")
            .arg(self.root.join("absent-herdr"))
            .args(["chat", "reply", "--bridge-state"])
            .arg(&self.state)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout_path).expect("create standard output"))
            .stderr(fs::File::create(&stderr_path).expect("create standard error"))
            .spawn()
            .expect("run agentctl");
        let status = wait_at_most(&mut child, RUN_LIMIT);
        let read = |path: &Path| {
            String::from_utf8(fs::read(path).expect("read output")).expect("UTF-8 output")
        };
        let (stdout, stderr) = (read(&stdout_path), read(&stderr_path));
        let status = status.unwrap_or_else(|| {
            panic!("chat reply ran longer than {RUN_LIMIT:?} and was killed: {stderr}")
        });
        assert!(!registry.exists(), "chat reply created a registry");
        Run {
            code: status.code(),
            stdout,
            stderr,
        }
    }

    /// Run `chat reply` for this state's request with `reply_id` and the reply file `file`.
    fn reply_with(&self, reply_id: &str, file: &str) -> Run {
        self.reply(&[
            "--request",
            &self.key,
            "--reply-id",
            reply_id,
            "--file",
            file,
        ])
    }

    /// The JSON object that a run storing a reply of this state's request prints.
    fn result(&self, outcome: &str, ordinal: u32) -> Value {
        json!({
            "request": self.key,
            "reply_id": self.reply_id,
            "outcome": outcome,
            "ordinal": ordinal,
            "phase": "pending",
            "service_woken": false,
        })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Wait at most `limit` for `child` to exit. Past that, kill it and return `None`.
fn wait_at_most(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().expect("poll agentctl") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Check that `run` printed one JSON object equal to `expected` and nothing on standard error.
fn assert_stored(run: &Run, expected: &Value) {
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stderr, "");
    let printed: Value = serde_json::from_str(&run.stdout).expect("one JSON object");
    assert_eq!(&printed, expected);
}

/// Check that `run` exited `code`, printed nothing on standard output, and named `reason` on
/// standard error.
fn assert_failed(run: &Run, code: i32, reason: &str) {
    assert_eq!(run.code, Some(code), "{}", run.stderr);
    assert_eq!(run.stdout, "");
    assert!(run.stderr.contains(reason), "{}", run.stderr);
}

#[test]
fn chat_reply_exits_0_with_the_stored_reply_and_stores_a_text_once() {
    let scratch = Scratch::new("stored");
    let reply = scratch.file("reply.md", b"answer\n");
    assert_stored(
        &scratch.reply_with(&scratch.reply_id, &reply),
        &scratch.result("stored", 1),
    );
    assert_stored(
        &scratch.reply_with(&scratch.reply_id, &reply),
        &scratch.result("already_stored", 1),
    );
    // The agent label that the service puts before every reply it sends counts toward the
    // 30,000 bytes.
    let longest = scratch.file(
        "longest.md",
        "x".repeat(30_000 - "[worker] ".len()).as_bytes(),
    );
    assert_stored(
        &scratch.reply_with(&scratch.reply_id, &longest),
        &scratch.result("stored", 2),
    );
}

#[test]
fn chat_reply_exits_1_for_a_refused_reply_and_2_for_an_unusable_file() {
    let scratch = Scratch::new("refused");
    let reply = scratch.file("reply.md", b"answer\n");
    assert_failed(
        &scratch.reply_with("anything", &reply),
        1,
        "the reply ID is not one that this request's prompt gives",
    );
    let unlabelled = scratch.file("unlabelled.md", "x".repeat(30_000).as_bytes());
    assert_failed(
        &scratch.reply_with(&scratch.reply_id, &unlabelled),
        1,
        "agent-labelled chat reply exceeds 30000 UTF-8 bytes",
    );
    assert_failed(
        &scratch.reply(&[
            "--request",
            "NOT-A-KEY",
            "--reply-id",
            &scratch.reply_id,
            "--file",
            &reply,
        ]),
        1,
        "chat request key must be 64 lowercase hexadecimal characters",
    );
    assert_failed(
        &scratch.reply(&["--request", &scratch.key, "--reply-id", &scratch.reply_id]),
        2,
        "--file",
    );
    let absent = scratch.root.join("absent.md");
    assert_failed(
        &scratch.reply_with(&scratch.reply_id, absent.to_str().expect("UTF-8 path")),
        2,
        "cannot read",
    );
    // An empty, blank or over-long text is a refused reply; a file that is not UTF-8 is unusable.
    for (name, contents, status, reason) in [
        (
            "empty.md",
            b"".to_vec(),
            1,
            "chat reply body must be nonempty",
        ),
        (
            "blank.md",
            b" \n\n".to_vec(),
            1,
            "chat reply body must be nonempty",
        ),
        (
            "long.md",
            format!("{}\n", "x".repeat(30_000)).into_bytes(),
            1,
            "chat reply exceeds 30000 UTF-8 bytes",
        ),
        (
            "binary.md",
            b"answer \xff\n".to_vec(),
            2,
            "does not contain valid UTF-8",
        ),
    ] {
        let file = scratch.file(name, &contents);
        assert_failed(
            &scratch.reply_with(&scratch.reply_id, &file),
            status,
            reason,
        );
    }
    // None of those stored anything.
    assert_stored(
        &scratch.reply_with(&scratch.reply_id, &reply),
        &scratch.result("stored", 1),
    );
}

#[test]
fn chat_reply_exits_75_when_a_short_reply_id_waits_for_an_unusable_alias_record() {
    let scratch = Scratch::new("try-again");
    let reply = scratch.file("reply.md", b"answer\n");
    fs::write(scratch.state.join("reply-aliases.json"), "{").expect("spoil the alias record");
    assert_failed(&scratch.reply_with("001", &reply), 75, "is unusable");
    // Nothing was stored, and a long reply ID does not depend on the record.
    assert_stored(
        &scratch.reply_with(&scratch.reply_id, &reply),
        &scratch.result("stored", 1),
    );
}
