//! Read and update native Codex goals through the app-server JSON-RPC protocol.
//!
//! The default command, `codex app-server proxy`, attaches to an existing local app-server
//! daemon. An explicit `codex app-server --stdio` can query saved goals on versions that expose
//! them without loading the thread. No operation here starts, resumes, or submits input to a
//! thread. Updating a goal through a separate server does not by itself promise to wake another
//! process that owns that thread.

// Every message, limit and error text follows the other edition's agentctl.codex_goal module, so
// `agentctl codex-goal` prints what `python -m agentctl.codex_goal` printed.

pub(crate) mod command;
mod ordered_json;

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use ordered_json::Json;

/// Transport that attaches to the local app-server daemon owning live threads.
pub const DEFAULT_COMMAND: [&str; 3] = ["codex", "app-server", "proxy"];
/// Separate app-server process selected by `agentctl codex-goal --stdio`.
pub const STDIO_COMMAND: [&str; 3] = ["codex", "app-server", "--stdio"];
/// Seconds allowed for one whole exchange when the caller names no timeout.
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 30.0;
/// Statuses a native goal can have, in the protocol's spelling.
pub const STATUSES: [&str; 6] = [
    "active",
    "paused",
    "blocked",
    "usageLimited",
    "budgetLimited",
    "complete",
];
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const STDERR_TAIL_BYTES: usize = 4096;
/// After the transport closes stdout, how long its stderr may take to reach end of file so the
/// refusal can quote it.
const STDERR_GRACE: Duration = Duration::from_millis(250);
/// How long a terminated transport may take to exit before its group is killed.
const TERMINATE_GRACE: Duration = Duration::from_millis(500);

/// Fields to change on a native goal. `None` leaves a field as the server has it.
#[derive(Clone, Copy, Debug, Default)]
pub struct GoalUpdate<'a> {
    /// New objective text, sent literally; it must not be blank.
    pub objective: Option<&'a str>,
    /// New status, one of [`STATUSES`].
    pub status: Option<&'a str>,
    /// New token budget, a positive integer.
    pub token_budget: Option<i64>,
}

enum Event {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Closed(bool),
    Error(String),
}

fn read_stream(mut stream: impl Read + Send + 'static, stdout: bool, sender: SyncSender<Event>) {
    // Reader threads are detached: a descendant that escaped the transport's process group can
    // keep a pipe open indefinitely, and the exchange must not wait for that end of file.
    thread::spawn(move || {
        let mut buffer = [0_u8; 65_536];
        loop {
            let event = match stream.read(&mut buffer) {
                Ok(0) => {
                    let _ = sender.send(Event::Closed(stdout));
                    return;
                }
                Ok(count) if stdout => Event::Stdout(buffer[..count].to_vec()),
                Ok(count) => Event::Stderr(buffer[..count].to_vec()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = sender.send(Event::Error(format!(
                        "invalid Codex goal protocol: {}",
                        os_error_text(&error)
                    )));
                    return;
                }
            };
            if sender.send(event).is_err() {
                return;
            }
        }
    });
}

/// An OS error written as `[Errno N] description`.
fn os_error_text(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => {
            let text = error.to_string();
            let suffix = format!(" (os error {code})");
            format!(
                "[Errno {code}] {}",
                text.strip_suffix(&suffix).unwrap_or(&text)
            )
        }
        None => error.to_string(),
    }
}

/// A failure inside one exchange. A reply of the wrong shape is reported with the
/// `invalid Codex goal protocol:` prefix.
enum RpcError {
    Goal(String),
    Protocol(String),
}

impl RpcError {
    fn into_message(self) -> String {
        match self {
            Self::Goal(message) => message,
            Self::Protocol(message) => format!("invalid Codex goal protocol: {message}"),
        }
    }
}

impl From<String> for RpcError {
    fn from(message: String) -> Self {
        Self::Goal(message)
    }
}

/// One bounded protocol connection, including launcher process cleanup.
struct Rpc {
    child: Child,
    input: ChildStdin,
    events: Option<Receiver<Event>>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_closed: bool,
    stderr_closed: bool,
    deadline: Instant,
    timeout_seconds: f64,
}

impl Drop for Rpc {
    fn drop(&mut self) {
        // Release readers blocked on the bounded channel.
        self.events.take();
        // A proxy's separately running daemon is not in this new session. A launcher and its
        // helpers are, and must not survive a timeout. The leader is observed with WNOWAIT and
        // reaped only after the final group kill, so its group ID cannot be reused meanwhile.
        let group = libc::pid_t::try_from(self.child.id()).unwrap_or(0);
        if group > 0 {
            // SAFETY: kill receives a negative group ID owned by this unreaped child.
            unsafe { libc::kill(-group, libc::SIGTERM) };
            let deadline = Instant::now() + TERMINATE_GRACE;
            while Instant::now() < deadline && !leader_exited(group) {
                thread::sleep(Duration::from_millis(5));
            }
            // SAFETY: as above; the leader is still unreaped.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        let _ = self.child.wait();
    }
}

/// Whether the child `pid` has exited, without reaping it.
fn leader_exited(pid: libc::pid_t) -> bool {
    // SAFETY: waitid writes only into the zeroed siginfo owned by this frame.
    let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let status = unsafe {
        libc::waitid(
            libc::P_PID,
            libc::id_t::try_from(pid).unwrap_or(0),
            &mut information,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    // SAFETY: si_pid is valid for the SIGCHLD information waitid reports.
    status == 0 && unsafe { information.si_pid() } != 0
}

impl Rpc {
    fn new(command: &[String], timeout_seconds: f64) -> Result<Self, String> {
        if !timeout_seconds.is_finite() || timeout_seconds <= 0.0 {
            return Err("timeout must be positive and finite".to_owned());
        }
        if command.is_empty()
            || command
                .iter()
                .any(|part| part.is_empty() || part.contains('\0'))
        {
            return Err("command must be a nonempty argument vector".to_owned());
        }
        let deadline = Duration::try_from_secs_f64(timeout_seconds)
            .ok()
            .and_then(|timeout| Instant::now().checked_add(timeout))
            .ok_or_else(|| "timeout is too large".to_owned())?;
        let mut process = Command::new(&command[0]);
        process
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
        unsafe {
            process.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = process
            .spawn()
            .map_err(|error| format!("cannot start Codex goal transport: {error}"))?;
        let input = child.stdin.take().expect("requested piped stdin");
        let stdout = child.stdout.take().expect("requested piped stdout");
        let stderr = child.stderr.take().expect("requested piped stderr");
        let (sender, receiver) = mpsc::sync_channel(16);
        read_stream(stdout, true, sender.clone());
        read_stream(stderr, false, sender);
        let rpc = Self {
            child,
            input,
            events: Some(receiver),
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_closed: false,
            stderr_closed: false,
            deadline,
            timeout_seconds,
        };
        // A transport that never reads stdin must not turn writing into an unbounded wait.
        // fcntl changes only our owned pipe descriptor.
        let fd = rpc.input.as_raw_fd();
        // SAFETY: fcntl reads and sets flags of a descriptor this process owns.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(format!(
                "cannot configure Codex goal transport: {}",
                os_error_text(&io::Error::last_os_error())
            ));
        }
        Ok(rpc)
    }

    fn remaining(&self) -> Result<Duration, String> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| self.timed_out())
    }

    fn timed_out(&self) -> String {
        format!(
            "Codex goal RPC timed out after {} seconds",
            ordered_json::format_g(self.timeout_seconds)
        )
    }

    fn send(&mut self, message: &Json) -> Result<(), String> {
        let mut bytes = message.dumps().into_bytes();
        bytes.push(b'\n');
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err("Codex goal request exceeds the message size limit".to_owned());
        }
        let mut offset = 0;
        while offset < bytes.len() {
            let remaining = self.remaining()?;
            match self.input.write(&bytes[offset..]) {
                Ok(0) => {
                    return Err(format!(
                        "cannot write Codex goal request: {}",
                        os_error_text(&io::Error::from_raw_os_error(libc::EPIPE))
                    ))
                }
                Ok(count) => offset += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(remaining.min(Duration::from_millis(5)));
                }
                Err(error) => {
                    return Err(format!(
                        "cannot write Codex goal request: {}",
                        os_error_text(&error)
                    ))
                }
            }
        }
        Ok(())
    }

    fn apply(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::Stdout(bytes) => {
                self.stdout.extend(bytes);
                if self.stdout.len() > MAX_MESSAGE_BYTES {
                    return Err("Codex goal response exceeds the message size limit".to_owned());
                }
            }
            Event::Stderr(bytes) => {
                self.stderr.extend(bytes);
                if self.stderr.len() > STDERR_TAIL_BYTES {
                    self.stderr.drain(..self.stderr.len() - STDERR_TAIL_BYTES);
                }
            }
            Event::Closed(true) => self.stdout_closed = true,
            Event::Closed(false) => self.stderr_closed = true,
            Event::Error(error) => return Err(error),
        }
        Ok(())
    }

    fn closed_before_reply(&mut self) -> String {
        // Let stderr reach end of file briefly, so a launcher's own explanation is quoted.
        let grace = Instant::now() + STDERR_GRACE;
        while !self.stderr_closed {
            let wait = grace
                .min(self.deadline)
                .saturating_duration_since(Instant::now());
            if wait.is_zero() {
                break;
            }
            match self
                .events
                .as_ref()
                .expect("live receiver")
                .recv_timeout(wait)
            {
                Ok(Event::Stderr(bytes)) => {
                    let _ = self.apply(Event::Stderr(bytes));
                }
                Ok(Event::Closed(false)) => self.stderr_closed = true,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let detail = String::from_utf8_lossy(&self.stderr);
        let detail = ordered_json::strip_space(&detail);
        if detail.is_empty() {
            "Codex goal transport closed before replying".to_owned()
        } else {
            format!("Codex goal transport closed before replying: {detail}")
        }
    }

    /// Return the result object of response `request_id`, skipping notifications.
    fn receive(&mut self, request_id: i64) -> Result<Json, RpcError> {
        loop {
            let remaining = self.remaining()?;
            if let Some(end) = self.stdout.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.stdout.drain(..=end).collect();
                let response = Json::parse(&line)
                    .map_err(|error| format!("invalid Codex goal response: {error}"))?;
                if !response.is_object() {
                    return Err(RpcError::Goal(format!(
                        "invalid Codex goal response: Codex goal response: expected an object, got {}",
                        response.type_name()
                    )));
                }
                if response.contains_key("method") {
                    if response.contains_key("id") {
                        return Err(RpcError::Goal(
                            "Codex requested an interactive action during goal RPC; no approval was sent"
                                .to_owned(),
                        ));
                    }
                    continue;
                }
                if !response
                    .get("id")
                    .is_some_and(|id| id.is_int_equal(request_id))
                {
                    return Err(RpcError::Goal(
                        "Codex returned an unexpected response ID".to_owned(),
                    ));
                }
                if let Some(error) = response.get("error") {
                    if !error.is_object() {
                        return Err(RpcError::Protocol(format!(
                            "Codex RPC error: expected an object, got {}",
                            error.type_name()
                        )));
                    }
                    return Err(RpcError::Goal(format!(
                        "Codex goal RPC refused: {}",
                        error.get("message").unwrap_or(error).message_text()
                    )));
                }
                return match response.get("result") {
                    Some(result) if result.is_object() => Ok(result.clone()),
                    other => Err(RpcError::Protocol(format!(
                        "Codex goal result: expected an object, got {}",
                        other.map_or("NoneType", Json::type_name)
                    ))),
                };
            }
            if self.stdout_closed {
                return Err(RpcError::Goal(self.closed_before_reply()));
            }
            let event = self
                .events
                .as_ref()
                .expect("live receiver")
                .recv_timeout(remaining)
                .map_err(|error| match error {
                    mpsc::RecvTimeoutError::Timeout => self.timed_out(),
                    mpsc::RecvTimeoutError::Disconnected => {
                        "Codex goal transport disconnected".to_owned()
                    }
                })?;
            self.apply(event)?;
        }
    }
}

/// Run one request through a new transport and return its result object.
fn call(method: &str, params: Json, command: &[String], timeout: f64) -> Result<Json, String> {
    let mut rpc = Rpc::new(command, timeout)?;
    let exchange = (|| -> Result<Json, RpcError> {
        rpc.send(&Json::object([
            ("id", Json::Int(1)),
            ("method", Json::Str("initialize".to_owned())),
            (
                "params",
                Json::object([
                    (
                        "clientInfo",
                        Json::object([
                            ("name", Json::Str("herdr-goal".to_owned())),
                            ("version", Json::Str("1".to_owned())),
                        ]),
                    ),
                    (
                        "capabilities",
                        Json::object([("experimentalApi", Json::Bool(true))]),
                    ),
                ]),
            ),
        ]))?;
        rpc.receive(1)?;
        rpc.send(&Json::object([
            ("method", Json::Str("initialized".to_owned())),
            ("params", Json::object([])),
        ]))?;
        rpc.send(&Json::object([
            ("id", Json::Int(2)),
            ("method", Json::Str(method.to_owned())),
            ("params", params),
        ]))?;
        rpc.receive(2)
    })();
    exchange.map_err(RpcError::into_message)
}

fn session_params(session_id: &str) -> Result<Json, String> {
    if session_id.is_empty() || session_id.contains('\0') {
        return Err("session_id must be a nonempty session identifier".to_owned());
    }
    Ok(Json::object([(
        "threadId",
        Json::Str(session_id.to_owned()),
    )]))
}

fn field_str<'v>(goal: &'v Json, key: &str) -> Result<&'v str, String> {
    goal.get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| format!("native Codex goal: field '{key}' is not a string"))
}

fn field_int(goal: &Json, key: &str) -> Result<(), String> {
    if goal.get(key).is_some_and(Json::is_int) {
        Ok(())
    } else {
        Err(format!(
            "native Codex goal: field '{key}' is not an integer"
        ))
    }
}

/// Validate a goal object: it belongs to `session_id`, has a known status and integer counters.
fn checked_goal(value: Option<&Json>, session_id: &str) -> Result<Json, String> {
    let goal = value.unwrap_or(&Json::Null);
    if !goal.is_object() {
        return Err(format!(
            "native Codex goal: expected an object, got {}",
            goal.type_name()
        ));
    }
    if field_str(goal, "threadId")? != session_id {
        return Err("native Codex goal belongs to another session".to_owned());
    }
    field_str(goal, "objective")?;
    if !STATUSES.contains(&field_str(goal, "status")?) {
        return Err("native Codex goal has an unknown status".to_owned());
    }
    for key in ["tokensUsed", "timeUsedSeconds", "createdAt", "updatedAt"] {
        field_int(goal, key)?;
    }
    if goal
        .get("tokenBudget")
        .is_some_and(|budget| !budget.is_null())
    {
        field_int(goal, "tokenBudget")?;
    }
    Ok(goal.clone())
}

fn timeout_seconds(timeout: Duration) -> f64 {
    timeout.as_secs_f64()
}

fn get_goal_json(
    session_id: &str,
    command: &[String],
    timeout: f64,
) -> Result<Option<Json>, String> {
    let response = call(
        "thread/goal/get",
        session_params(session_id)?,
        command,
        timeout,
    )?;
    match response.get("goal") {
        None | Some(Json::Null) => Ok(None),
        value => checked_goal(value, session_id).map(Some),
    }
}

fn set_goal_json(
    session_id: &str,
    update: &GoalUpdate<'_>,
    command: &[String],
    timeout: f64,
) -> Result<Json, String> {
    let mut params = session_params(session_id)?;
    let mut changed = false;
    if let Some(objective) = update.objective {
        if ordered_json::strip_space(objective).is_empty() {
            return Err("objective must not be blank".to_owned());
        }
        params.push("objective", Json::Str(objective.to_owned()));
        changed = true;
    }
    if let Some(status) = update.status {
        if !STATUSES.contains(&status) {
            return Err("unsupported native goal status".to_owned());
        }
        params.push("status", Json::Str(status.to_owned()));
        changed = true;
    }
    if let Some(budget) = update.token_budget {
        if budget <= 0 {
            return Err("token_budget must be a positive integer".to_owned());
        }
        params.push("tokenBudget", Json::Int(budget));
        changed = true;
    }
    if !changed {
        return Err("provide an objective, status, or token budget".to_owned());
    }
    let response = call("thread/goal/set", params, command, timeout)?;
    checked_goal(response.get("goal"), session_id)
}

fn clear_goal_json(session_id: &str, command: &[String], timeout: f64) -> Result<(), String> {
    call(
        "thread/goal/clear",
        session_params(session_id)?,
        command,
        timeout,
    )
    .map(drop)
}

/// Return a native goal or `None` without starting or resuming the target thread.
///
/// `command` is a shell-free argv, normally [`DEFAULT_COMMAND`], or an explicitly selected
/// stdio server. The entire start, handshake and read share one deadline; errors and timeouts
/// terminate only the launched transport's session.
pub fn get_goal(
    session_id: &str,
    command: &[String],
    timeout: Duration,
) -> Result<Option<Value>, String> {
    get_goal_json(session_id, command, timeout_seconds(timeout))
        .map(|goal| goal.as_ref().map(Json::to_value))
}

/// Update a native goal, sending only the fields `update` provides, and return the new goal.
///
/// A transport attached to the owning server delivers live goal notifications. A separate
/// stdio server can update saved state but does not promise to wake the thread's process.
pub fn set_goal(
    session_id: &str,
    update: &GoalUpdate<'_>,
    command: &[String],
    timeout: Duration,
) -> Result<Value, String> {
    set_goal_json(session_id, update, command, timeout_seconds(timeout)).map(|goal| goal.to_value())
}

/// Clear a saved goal without submitting a prompt or resuming the thread.
pub fn clear_goal(session_id: &str, command: &[String], timeout: Duration) -> Result<(), String> {
    clear_goal_json(session_id, command, timeout_seconds(timeout))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    /// A JSON-RPC goal server written in POSIX shell: it records every request line, answers
    /// `initialize`, sends a notification before each reply, and answers goal requests
    /// according to its mode.
    pub(crate) const SERVER: &str = r#"
defaults='"objective": "Observe progress", "status": "active", "tokenBudget": null, "tokensUsed": 10, "timeUsedSeconds": 2, "createdAt": 100, "updatedAt": 101'
reply() {
  if [ "$2" = fragmented ]; then
    printf '%s' "$1" | cut -c1-7 | tr -d '\n'
    sleep 0.01
    printf '%s\n' "$1" | cut -c8-
  else
    printf '%s\n' "$1"
  fi
}
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$1/calls.jsonl"
  case "$line" in
    *'"method": "initialized"'*) continue ;;
    *'"method": "initialize"'*) reply '{"id": 1, "result": {"userAgent": "fixture"}}' "$2"; continue ;;
  esac
  case "$2" in
    hang) sleep 60 & printf '%s' "$!" > "$1/child.tmp" && mv "$1/child.tmp" "$1/child.pid"; wait; continue ;;
    error) reply '{"id": 2, "error": {"code": -32601, "message": "unknown goal method"}}' "$2"; continue ;;
    invalid) printf '%s\n' 'invalid-json'; continue ;;
    interactive) reply '{"id": 98, "method": "item/requestApproval", "params": {}}' "$2"; continue ;;
    stderr) i=0; while [ "$i" -lt 20000 ]; do printf '%s\n' 'launcher-log' >&2; i=$((i+1)); done ;;
    closed) printf '%s\n' 'fixture transport failed' >&2; exit 4 ;;
  esac
  reply '{"method": "thread/goal/updated", "params": {}}' "$2"
  params=${line#*'"params": {'}
  params=${params%'}}'}
  case "$line" in
    *'"method": "thread/goal/clear"'*) result='{}' ;;
    *)
      case "$2" in
        absent) result='{"goal": null}' ;;
        wrong-session) result="{\"goal\": {$defaults, $params, \"threadId\": \"unrelated-session\"}}" ;;
        bad-counter) result="{\"goal\": {$defaults, $params, \"tokensUsed\": true}}" ;;
        *) result="{\"goal\": {$defaults, $params}}" ;;
      esac
      ;;
  esac
  id=2
  if [ "$2" = wrong-id ]; then id=99; fi
  reply "{\"id\": $id, \"result\": $result}" "$2"
done
"#;

    pub(crate) struct Fixture(pub(crate) PathBuf);

    impl Fixture {
        pub(crate) fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "herdr-goal-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub(crate) fn command(&self, mode: &str) -> Vec<String> {
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                SERVER.to_owned(),
                "goal-fixture".to_owned(),
                self.0.to_string_lossy().into_owned(),
                mode.to_owned(),
            ]
        }

        pub(crate) fn calls(&self) -> Vec<Value> {
            calls(&self.0)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn calls(root: &Path) -> Vec<Value> {
        fs::read_to_string(root.join("calls.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn read_has_no_resume_or_turn_side_effects() {
        for mode in ["normal", "fragmented", "stderr"] {
            let fixture = Fixture::new();
            let goal = get_goal("thread-fixture", &fixture.command(mode), TIMEOUT)
                .unwrap()
                .unwrap();
            assert_eq!(goal["objective"], "Observe progress", "{mode}");
            assert_eq!(goal["tokensUsed"], 10, "{mode}");
            let calls = fixture.calls();
            assert_eq!(
                calls
                    .iter()
                    .map(|call| call["method"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec!["initialize", "initialized", "thread/goal/get"]
            );
            assert_eq!(
                calls[2]["params"],
                serde_json::json!({"threadId":"thread-fixture"})
            );
        }
    }

    #[test]
    fn requests_use_the_python_editions_bytes() {
        let fixture = Fixture::new();
        get_goal("thread-fixture", &fixture.command("normal"), TIMEOUT).unwrap();
        let recorded = fs::read_to_string(fixture.0.join("calls.jsonl")).unwrap();
        assert_eq!(
            recorded,
            concat!(
                r#"{"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "herdr-goal", "version": "1"}, "capabilities": {"experimentalApi": true}}}"#,
                "\n",
                r#"{"method": "initialized", "params": {}}"#,
                "\n",
                r#"{"id": 2, "method": "thread/goal/get", "params": {"threadId": "thread-fixture"}}"#,
                "\n"
            )
        );
    }

    #[test]
    fn absent_goal_is_distinct_from_protocol_failure() {
        let fixture = Fixture::new();
        assert!(
            get_goal("thread-fixture", &fixture.command("absent"), TIMEOUT)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn set_omits_unspecified_fields_and_preserves_literal_text() {
        let fixture = Fixture::new();
        let objective = "Inspect $(never-execute) and `literal`\nThen report \u{3c0}.";
        let goal = set_goal(
            "thread-fixture",
            &GoalUpdate {
                objective: Some(objective),
                ..GoalUpdate::default()
            },
            &fixture.command("normal"),
            TIMEOUT,
        )
        .unwrap();
        assert_eq!(goal["objective"], objective);
        assert_eq!(
            fixture.calls().last().unwrap(),
            &serde_json::json!({
                "id": 2,
                "method": "thread/goal/set",
                "params": {"threadId": "thread-fixture", "objective": objective},
            })
        );
    }

    #[test]
    fn set_status_and_budget_does_not_replace_objective() {
        let fixture = Fixture::new();
        let goal = set_goal(
            "thread-fixture",
            &GoalUpdate {
                status: Some("paused"),
                token_budget: Some(100),
                ..GoalUpdate::default()
            },
            &fixture.command("normal"),
            TIMEOUT,
        )
        .unwrap();
        assert_eq!(goal["status"], "paused");
        assert_eq!(goal["tokenBudget"], 100);
        assert_eq!(
            fixture.calls().last().unwrap()["params"],
            serde_json::json!({"threadId": "thread-fixture", "status": "paused", "tokenBudget": 100})
        );
    }

    #[test]
    fn clear_is_a_single_goal_operation() {
        let fixture = Fixture::new();
        clear_goal("thread-fixture", &fixture.command("normal"), TIMEOUT).unwrap();
        assert_eq!(
            fixture.calls().last().unwrap()["method"],
            "thread/goal/clear"
        );
    }

    #[test]
    fn protocol_failures_are_actionable() {
        let fixture = Fixture::new();
        for (mode, message) in [
            ("error", "Codex goal RPC refused: unknown goal method"),
            ("invalid", "invalid Codex goal response"),
            ("interactive", "no approval was sent"),
            (
                "closed",
                "Codex goal transport closed before replying: fixture transport failed",
            ),
            (
                "wrong-session",
                "native Codex goal belongs to another session",
            ),
            (
                "bad-counter",
                "native Codex goal: field 'tokensUsed' is not an integer",
            ),
            ("wrong-id", "Codex returned an unexpected response ID"),
        ] {
            let error = get_goal("thread-fixture", &fixture.command(mode), TIMEOUT).unwrap_err();
            assert!(error.contains(message), "{mode}: {error}");
        }
    }

    #[test]
    fn set_requires_a_non_null_goal_response() {
        let fixture = Fixture::new();
        let error = set_goal(
            "thread-fixture",
            &GoalUpdate {
                objective: Some("An objective"),
                ..GoalUpdate::default()
            },
            &fixture.command("absent"),
            TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(error, "native Codex goal: expected an object, got NoneType");
    }

    #[test]
    fn invalid_updates_are_refused_before_starting_transport() {
        let fixture = Fixture::new();
        let command = fixture.command("normal");
        let update = |update: GoalUpdate<'_>| {
            set_goal("thread-fixture", &update, &command, TIMEOUT).unwrap_err()
        };
        assert_eq!(
            update(GoalUpdate::default()),
            "provide an objective, status, or token budget"
        );
        assert_eq!(
            update(GoalUpdate {
                objective: Some("  \u{1c}"),
                ..GoalUpdate::default()
            }),
            "objective must not be blank"
        );
        assert_eq!(
            update(GoalUpdate {
                status: Some("invented"),
                ..GoalUpdate::default()
            }),
            "unsupported native goal status"
        );
        for budget in [0, -1] {
            assert_eq!(
                update(GoalUpdate {
                    token_budget: Some(budget),
                    ..GoalUpdate::default()
                }),
                "token_budget must be a positive integer"
            );
        }
        assert!(!fixture.0.join("calls.jsonl").exists());
    }

    #[test]
    fn invalid_sessions_timeouts_and_commands_use_python_messages() {
        let fixture = Fixture::new();
        let command = fixture.command("normal");
        assert_eq!(
            get_goal("", &command, TIMEOUT).unwrap_err(),
            "session_id must be a nonempty session identifier"
        );
        for timeout in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                get_goal_json("thread-fixture", &command, timeout).unwrap_err(),
                "timeout must be positive and finite"
            );
        }
        for bad in [vec![], vec![String::new()], vec!["co\0dex".to_owned()]] {
            assert_eq!(
                get_goal("thread-fixture", &bad, TIMEOUT).unwrap_err(),
                "command must be a nonempty argument vector"
            );
        }
        assert!(!fixture.0.join("calls.jsonl").exists());
    }

    #[test]
    fn timeout_reaps_transport_descendants() {
        // The deadline starts before the transport shell runs, so a loaded host can expire it
        // before the fixture publishes its descendant. Such an attempt never exercised a
        // descendant; retry it with a longer deadline rather than read an unpublished marker.
        for timeout in [0.5, 1.0, 3.0, 10.0] {
            let fixture = Fixture::new();
            let start = Instant::now();
            let error =
                get_goal_json("thread-fixture", &fixture.command("hang"), timeout).unwrap_err();
            assert_eq!(
                error,
                format!(
                    "Codex goal RPC timed out after {} seconds",
                    ordered_json::format_g(timeout)
                )
            );
            assert!(start.elapsed() < Duration::from_secs_f64(timeout) + Duration::from_secs(5));
            let Ok(pid) = fs::read_to_string(fixture.0.join("child.pid")) else {
                continue;
            };
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                match fs::read_to_string(format!("/proc/{pid}/stat")) {
                    Err(_) => return,
                    Ok(stat)
                        if stat
                            .rsplit_once(") ")
                            .is_some_and(|(_, tail)| tail.starts_with('Z')) =>
                    {
                        return
                    }
                    Ok(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
            panic!("timed-out transport child {pid} remains running");
        }
        panic!("the hang fixture never published its descendant before any deadline");
    }

    #[test]
    fn timeout_message_uses_pythons_general_number_format() {
        assert_eq!(ordered_json::format_g(0.5), "0.5");
        assert_eq!(ordered_json::format_g(30.0), "30");
    }

    #[test]
    fn missing_transport_uses_cross_language_os_error() {
        let error = get_goal(
            "thread-fixture",
            &["/definitely/missing/codex-goal-transport".to_owned()],
            TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "cannot start Codex goal transport: No such file or directory (os error 2)"
        );
    }

    #[test]
    fn write_failures_use_pythons_os_error_text() {
        assert_eq!(
            os_error_text(&io::Error::from_raw_os_error(libc::EPIPE)),
            "[Errno 32] Broken pipe"
        );
    }

    #[test]
    fn non_object_rpc_errors_and_results_are_protocol_failures() {
        for (reply, message) in [
            (
                r#"{"id": 2, "error": "boom"}"#,
                "invalid Codex goal protocol: Codex RPC error: expected an object, got str",
            ),
            (
                r#"{"id": 2}"#,
                "invalid Codex goal protocol: Codex goal result: expected an object, got NoneType",
            ),
            (
                r#"{"id": 2, "result": [1]}"#,
                "invalid Codex goal protocol: Codex goal result: expected an object, got list",
            ),
            (
                r#"{"id": 2, "error": {"code": -1}}"#,
                "Codex goal RPC refused: {'code': -1}",
            ),
            (
                r#"[2]"#,
                "invalid Codex goal response: Codex goal response: expected an object, got list",
            ),
            (
                r#"{"id": true, "result": {}}"#,
                "Codex returned an unexpected response ID",
            ),
            (
                r#"{"id": 2.0, "result": {}}"#,
                "Codex returned an unexpected response ID",
            ),
        ] {
            let script = format!(
                "read -r _; printf '%s\\n' '{{\"id\": 1, \"result\": {{}}}}'; read -r _; read -r _; printf '%s\\n' '{reply}'; sleep 5"
            );
            let command = vec!["/bin/sh".to_owned(), "-c".to_owned(), script];
            let error = get_goal("thread-fixture", &command, TIMEOUT).unwrap_err();
            assert_eq!(error, message, "{reply}");
        }
    }
}
