//! `agentctl codex-goal`: native goal operations from the command line.
//!
//! Success prints one JSON line; failure prints `native goal: REASON` on stderr and exits 1.

// This replaces the other edition's `python -m agentctl.codex_goal`, with the same options,
// output and refusals.

use std::io::{self, Write as _};

use clap::{Args, Subcommand};

use super::ordered_json::Json;
use super::{GoalUpdate, DEFAULT_COMMAND, STATUSES, STDIO_COMMAND};

/// Examples and output contract shown after `agentctl codex-goal --help`.
pub(crate) const AFTER_HELP: &str = "Examples:
  agentctl codex-goal get THREAD_ID
  agentctl codex-goal --stdio get THREAD_ID
  agentctl codex-goal set THREAD_ID --objective 'Finish the review and report blockers'
  agentctl codex-goal set THREAD_ID --status paused --token-budget 200000
  agentctl codex-goal --command-json '[\"codex\",\"app-server\",\"proxy\"]' --timeout 10 clear THREAD_ID

SESSION_ID is the Codex thread ID (`agentctl status NAME` shows it as session_value). The
transport options come before the action. The default transport, `codex app-server proxy`,
attaches to the local app-server daemon that owns live threads, so a change reaches a running
TUI. No operation starts, resumes, or submits input to the thread.

On success one JSON line is printed: the goal object as Codex returned it, or null when get
finds no goal and after clear. On failure nothing is printed on stdout, `native goal: REASON`
is printed on stderr and the exit status is 1; usage errors exit 2. The launched transport and
any helpers it started are terminated when the exchange ends or times out.";

/// Options and action of `agentctl codex-goal`.
#[derive(Args)]
pub(crate) struct CodexGoalArgs {
    /// Use a separate `codex app-server --stdio` process instead of `codex app-server proxy`.
    /// It reads and updates saved goals, but does not promise to wake a process that owns the
    /// thread
    #[arg(long, conflicts_with = "command_json")]
    stdio: bool,
    /// Transport argv as a JSON array of strings, run without a shell, instead of the default
    /// `["codex","app-server","proxy"]`
    #[arg(long, value_name = "JSON")]
    command_json: Option<String>,
    /// Seconds allowed for the whole exchange (start, handshake, request and reply); must be
    /// positive and finite
    #[arg(
        long,
        value_name = "SECONDS",
        default_value = "30",
        allow_negative_numbers = true,
        value_parser = seconds_value
    )]
    timeout: f64,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Print the saved goal of SESSION_ID, or null when it has none
    Get(Session),
    /// Change the goal of SESSION_ID, sending only the fields given, and print the updated goal
    Set(Set),
    /// Clear the saved goal of SESSION_ID and print null
    Clear(Session),
}

#[derive(Args)]
struct Session {
    /// Codex thread ID whose goal is read or changed
    #[arg(value_name = "SESSION_ID")]
    session_id: String,
}

#[derive(Args)]
struct Set {
    #[command(flatten)]
    session: Session,
    /// New objective, sent literally; it must not be blank
    #[arg(long, value_name = "TEXT")]
    objective: Option<String>,
    /// New goal status
    #[arg(long, value_name = "STATUS", value_parser = STATUSES)]
    status: Option<String>,
    /// New token budget, a positive integer number of tokens
    #[arg(
        long,
        value_name = "TOKENS",
        allow_negative_numbers = true,
        value_parser = integer_value
    )]
    token_budget: Option<i64>,
}

/// Seconds as a decimal number; surrounding whitespace is ignored and `inf`/`nan` parse.
fn seconds_value(text: &str) -> Result<f64, String> {
    super::ordered_json::strip_space(text)
        .parse()
        .map_err(|_| format!("invalid float value: {text:?}"))
}

/// A decimal integer; surrounding whitespace and a sign are accepted.
fn integer_value(text: &str) -> Result<i64, String> {
    super::ordered_json::strip_space(text)
        .parse()
        .map_err(|_| format!("invalid int value: {text:?}"))
}

/// Decode `--command-json`, which must be a JSON array of strings.
fn command_from_json(text: &str) -> Result<Vec<String>, String> {
    let decoded = Json::parse(text.as_bytes()).map_err(|error| error.to_string())?;
    let parts = decoded
        .as_array()
        .ok_or_else(|| format!("command: expected an array, got {}", decoded.type_name()))?;
    parts
        .iter()
        .map(|part| part.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "command must contain only strings".to_owned())
}

fn execute(arguments: &CodexGoalArgs, default_command: &[&str]) -> Result<Json, String> {
    let selected: &[&str] = if arguments.stdio {
        &STDIO_COMMAND
    } else {
        default_command
    };
    let mut command: Vec<String> = selected.iter().map(|part| (*part).to_owned()).collect();
    if let Some(text) = &arguments.command_json {
        command = command_from_json(text)?;
    }
    let timeout = arguments.timeout;
    match &arguments.action {
        Action::Get(session) => super::get_goal_json(&session.session_id, &command, timeout)
            .map(|goal| goal.unwrap_or(Json::Null)),
        Action::Set(set) => super::set_goal_json(
            &set.session.session_id,
            &GoalUpdate {
                objective: set.objective.as_deref(),
                status: set.status.as_deref(),
                token_budget: set.token_budget,
            },
            &command,
            timeout,
        ),
        Action::Clear(session) => {
            super::clear_goal_json(&session.session_id, &command, timeout).map(|()| Json::Null)
        }
    }
}

/// Run `agentctl codex-goal` and return its exit status. Only a failure to write the result to
/// standard output is returned as an error.
pub(crate) fn run(arguments: &CodexGoalArgs) -> io::Result<i32> {
    run_with(arguments, &DEFAULT_COMMAND)
}

fn run_with(arguments: &CodexGoalArgs, default_command: &[&str]) -> io::Result<i32> {
    match execute(arguments, default_command) {
        Ok(result) => {
            let mut output = io::stdout().lock();
            output.write_all(result.dumps().as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
            Ok(0)
        }
        Err(message) => {
            eprintln!("native goal: {message}");
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_goal::tests::Fixture;
    use clap::{CommandFactory, FromArgMatches, Parser};

    #[derive(Parser)]
    #[command(name = "codex-goal")]
    struct Harness {
        #[command(flatten)]
        arguments: CodexGoalArgs,
    }

    fn parse(arguments: &[&str]) -> Result<CodexGoalArgs, clap::Error> {
        let matches = Harness::command()
            .try_get_matches_from(std::iter::once("codex-goal").chain(arguments.iter().copied()))?;
        Harness::from_arg_matches(&matches).map(|harness| harness.arguments)
    }

    #[test]
    fn default_connects_to_the_configured_transport() {
        let fixture = Fixture::new();
        let command = fixture.command("normal");
        let default: Vec<&str> = command.iter().map(String::as_str).collect();
        let arguments = parse(&["get", "thread-fixture"]).unwrap();
        let goal = execute(&arguments, &default).unwrap();
        assert_eq!(
            goal.get("threadId").and_then(Json::as_str),
            Some("thread-fixture")
        );
        assert_eq!(arguments.timeout, 30.0);
    }

    #[test]
    fn output_is_the_python_editions_json_line_in_server_order() {
        let fixture = Fixture::new();
        let command = serde_json::to_string(&fixture.command("normal")).unwrap();
        let arguments = parse(&["--command-json", &command, "get", "thread-fixture"]).unwrap();
        assert_eq!(
            execute(&arguments, &DEFAULT_COMMAND).unwrap().dumps(),
            r#"{"objective": "Observe progress", "status": "active", "tokenBudget": null, "tokensUsed": 10, "timeUsedSeconds": 2, "createdAt": 100, "updatedAt": 101, "threadId": "thread-fixture"}"#
        );
        let arguments = parse(&["--command-json", &command, "clear", "thread-fixture"]).unwrap();
        assert_eq!(
            execute(&arguments, &DEFAULT_COMMAND).unwrap().dumps(),
            "null"
        );
        let absent = serde_json::to_string(&fixture.command("absent")).unwrap();
        let arguments = parse(&["--command-json", &absent, "get", "thread-fixture"]).unwrap();
        assert_eq!(
            execute(&arguments, &DEFAULT_COMMAND).unwrap().dumps(),
            "null"
        );
    }

    #[test]
    fn set_sends_only_given_fields_and_prints_the_updated_goal() {
        let fixture = Fixture::new();
        let command = serde_json::to_string(&fixture.command("normal")).unwrap();
        let arguments = parse(&[
            "--command-json",
            &command,
            "set",
            "thread-fixture",
            "--status",
            "paused",
            "--token-budget",
            "100",
        ])
        .unwrap();
        let goal = execute(&arguments, &DEFAULT_COMMAND).unwrap();
        assert_eq!(goal.get("status").and_then(Json::as_str), Some("paused"));
        assert_eq!(
            fixture.calls().last().unwrap()["params"],
            serde_json::json!({"threadId": "thread-fixture", "status": "paused", "tokenBudget": 100})
        );
    }

    #[test]
    fn command_json_errors_use_python_messages() {
        for (text, message) in [
            (r#"{"a": 1}"#, "command: expected an array, got dict"),
            (r#""codex""#, "command: expected an array, got str"),
            (r#"["codex", 1]"#, "command must contain only strings"),
            ("[]", "command must be a nonempty argument vector"),
            (
                r#"["codex", ""]"#,
                "command must be a nonempty argument vector",
            ),
        ] {
            let arguments = parse(&["--command-json", text, "get", "thread"]).unwrap();
            assert_eq!(
                execute(&arguments, &DEFAULT_COMMAND).unwrap_err(),
                message,
                "{text}"
            );
        }
        let arguments = parse(&["--command-json", "[", "get", "thread"]).unwrap();
        assert!(execute(&arguments, &DEFAULT_COMMAND).is_err());
    }

    #[test]
    fn argument_validation_follows_the_python_parser() {
        let arguments = parse(&["--timeout", "-1", "get", "thread"]).unwrap();
        assert_eq!(arguments.timeout, -1.0);
        assert_eq!(
            execute(&arguments, &DEFAULT_COMMAND).unwrap_err(),
            "timeout must be positive and finite"
        );
        let arguments = parse(&["--timeout", " inf ", "get", "thread"]).unwrap();
        assert_eq!(
            execute(&arguments, &DEFAULT_COMMAND).unwrap_err(),
            "timeout must be positive and finite"
        );
        let arguments = parse(&["set", "thread", "--token-budget", "-1"]).unwrap();
        assert_eq!(
            execute(&arguments, &DEFAULT_COMMAND).unwrap_err(),
            "token_budget must be a positive integer"
        );
        let arguments = parse(&["set", "thread"]).unwrap();
        assert_eq!(
            execute(&arguments, &DEFAULT_COMMAND).unwrap_err(),
            "provide an objective, status, or token budget"
        );
        let arguments = parse(&["get", ""]).unwrap();
        assert_eq!(
            execute(&arguments, &DEFAULT_COMMAND).unwrap_err(),
            "session_id must be a nonempty session identifier"
        );
        for refused in [
            &["--stdio", "--command-json", "[\"x\"]", "get", "thread"][..],
            &["get", "thread", "--stdio"],
            &["set", "thread", "--status", "invented"],
            &["--timeout", "soon", "get", "thread"],
            &["set", "thread", "--token-budget", "1.5"],
            &["get"],
            &[],
        ] {
            let error = parse(refused)
                .err()
                .unwrap_or_else(|| panic!("{refused:?}"));
            assert_eq!(error.exit_code(), 2, "{refused:?}");
        }
        assert!(parse(&["--stdio", "get", "thread"]).unwrap().stdio);
    }

    #[test]
    fn help_describes_every_argument() {
        let mut command = Harness::command();
        let help = command.render_long_help().to_string();
        for text in ["--stdio", "--command-json", "--timeout", "[default: 30]"] {
            assert!(help.contains(text), "{text}: {help}");
        }
        let set = command
            .find_subcommand_mut("set")
            .unwrap()
            .render_long_help()
            .to_string();
        for text in [
            "--objective",
            "--status",
            "--token-budget",
            "usageLimited",
            "SESSION_ID",
        ] {
            assert!(set.contains(text), "{text}: {set}");
        }
    }
}
