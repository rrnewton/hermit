//! Command-line interface for durable interactive-agent messaging through Herdr.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::json;

use crate::agent::{self, AgentError, DrainOptions, QueueOutcome, Target};
use crate::client::HerdrClient;
use crate::error::AdapterError;
use crate::subagents::{
    stop_reason, ManagedAgents, RecoveryAction, StartOptions, StopContext, StopFailure, StopOptions,
};

const COMMANDS: [&str; 11] = [
    "start",
    "stop",
    "list",
    "wait",
    "goal",
    "bind-session",
    "send",
    "drain",
    "status",
    "read",
    "userguide",
];
const MAX_WAIT_SECONDS: f64 = 31_536_000.0;
const MAX_COUNT: u64 = 1_000_000;

#[derive(Clone, Debug, PartialEq)]
struct Args {
    positional: Vec<String>,
    file: Option<PathBuf>,
    pane: Option<String>,
    session_agent: Option<String>,
    session: Option<String>,
    expected_agent: Option<String>,
    expected_workspace: Option<String>,
    expected_cwd: Option<PathBuf>,
    queue: PathBuf,
    ready_timeout: f64,
    working_timeout: f64,
    max_attempts: u64,
    lines: usize,
    herdr_bin: PathBuf,
    userguide: bool,
    name: Option<String>,
    registry: PathBuf,
    workspace_id: Option<String>,
    harness: String,
    model: Option<String>,
    resume: Option<String>,
    harness_args: Vec<String>,
    brief: Option<String>,
    startup_timeout: f64,
    goal_command_json: Option<String>,
    expected_token: Option<String>,
    recover_legacy_adoption: bool,
    retire_dead_adoption: bool,
    expected_record_sha256: Option<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            positional: Vec::new(),
            file: None,
            pane: None,
            session_agent: None,
            session: None,
            expected_agent: None,
            expected_workspace: None,
            expected_cwd: None,
            queue: PathBuf::from(".herdr-agent"),
            ready_timeout: 900.0,
            working_timeout: 30.0,
            max_attempts: 3,
            lines: 500,
            herdr_bin: PathBuf::from("herdr"),
            userguide: false,
            name: None,
            registry: PathBuf::from(".herdr-agents"),
            workspace_id: None,
            harness: "codex".to_owned(),
            model: None,
            resume: None,
            harness_args: Vec::new(),
            brief: None,
            startup_timeout: 30.0,
            goal_command_json: None,
            expected_token: None,
            recover_legacy_adoption: false,
            retire_dead_adoption: false,
            expected_record_sha256: None,
        }
    }
}

enum ParseResult {
    Args(Box<Args>),
    Exit(i32),
}

struct ParseFailure {
    message: String,
    stop_context: Option<StopContext>,
}

/// Run the `herdr-agent` command over an argument iterator.
pub fn main<I>(arguments: I) -> i32
where
    I: IntoIterator<Item = OsString>,
{
    let parsed = match parse_with_context(arguments) {
        Ok(parsed) => parsed,
        Err(failure) => {
            eprintln!("usage: {}", usage_line());
            eprintln!("herdr-agent: error: {}", failure.message);
            if let Some(context) = &failure.stop_context {
                print_stop_recovery(context, &RecoveryAction::Doctor);
            }
            return 2;
        }
    };
    let args = match parsed {
        ParseResult::Args(args) => *args,
        ParseResult::Exit(code) => return code,
    };
    let stop_context = parsed_stop_context(&args);
    if let Err(message) = validate_positionals(&args) {
        eprintln!("usage: {}", usage_line());
        eprintln!("herdr-agent: error: {message}");
        if let Some(context) = &stop_context {
            print_stop_recovery(context, &RecoveryAction::Doctor);
        }
        return 2;
    }
    if args.userguide
        || args
            .positional
            .first()
            .is_some_and(|value| value == "userguide")
    {
        print!("{}", crate::AGENT_USER_GUIDE);
        return 0;
    }
    if args.positional.is_empty() {
        print_help();
        return 0;
    }
    if let Err(message) = validate(&args) {
        eprintln!("usage: {}", usage_line());
        eprintln!("herdr-agent: error: {message}");
        if let Some(context) = &stop_context {
            print_stop_recovery(context, &RecoveryAction::Doctor);
        }
        return 2;
    }
    match run(args) {
        Ok(code) => code,
        Err(CliError::Agent(error)) => {
            emit_agent_error(&error, stop_context.as_ref(), &RecoveryAction::Doctor)
        }
        Err(CliError::Stop(failure)) => {
            emit_agent_error(&failure.error, stop_context.as_ref(), &failure.recovery)
        }
        Err(CliError::Herdr(error)) => {
            if let Some(context) = &stop_context {
                eprintln!("herdr-agent: {}", stop_reason(&error));
                print_stop_recovery(context, &RecoveryAction::Doctor);
            } else {
                eprintln!("herdr-agent: {error}");
            }
            error.exit_code()
        }
        Err(CliError::Usage(message)) => {
            if let Some(context) = &stop_context {
                eprintln!("herdr-agent: {}", stop_reason(&message));
                print_stop_recovery(context, &RecoveryAction::Doctor);
            } else {
                eprintln!("herdr-agent: {message}");
            }
            2
        }
        Err(CliError::Output(error)) => {
            eprintln!("herdr-agent: cannot write output: {error}");
            if let Some(context) = &stop_context {
                print_stop_recovery(context, &RecoveryAction::Doctor);
            }
            1
        }
    }
}

#[derive(Debug)]
enum CliError {
    Agent(AgentError),
    Stop(StopFailure),
    Herdr(AdapterError),
    Usage(String),
    Output(io::Error),
}

impl From<AgentError> for CliError {
    fn from(error: AgentError) -> Self {
        Self::Agent(error)
    }
}

impl From<StopFailure> for CliError {
    fn from(error: StopFailure) -> Self {
        Self::Stop(error)
    }
}

impl From<AdapterError> for CliError {
    fn from(error: AdapterError) -> Self {
        Self::Herdr(error)
    }
}

fn run(args: Args) -> Result<i32, CliError> {
    let command = args
        .positional
        .first()
        .expect("validated command is present")
        .as_str();
    if args.retire_dead_adoption && command != "stop" {
        return Err(CliError::Usage(
            "--retire-dead-adoption is valid only with stop".to_owned(),
        ));
    }
    if args.name.is_some()
        || matches!(
            command,
            "start" | "stop" | "list" | "wait" | "goal" | "bind-session"
        )
    {
        return run_managed(args);
    }
    let message = if command == "send" {
        Some(message(&args)?)
    } else {
        None
    };
    let target = Target {
        pane_id: args.pane,
        session_agent: args.session_agent,
        session_value: args.session,
        expected_agent: args.expected_agent,
        expected_workspace: args.expected_workspace,
        expected_cwd: args.expected_cwd,
    };
    // Reject an empty target before resolving the environment-dependent Herdr executable.  This
    // keeps a malformed invocation deterministic on hosts that do and do not have Herdr installed.
    agent::validate_target_authority(&target)?;
    let client = HerdrClient::with_executable("direct", &args.herdr_bin)?;
    let queue = absolute_lexical(&args.queue).map_err(|error| {
        CliError::Usage(format!(
            "cannot resolve queue path {}: {error}",
            args.queue.display()
        ))
    })?;
    let options = DrainOptions {
        ready_timeout: Duration::from_secs_f64(args.ready_timeout),
        working_timeout: Duration::from_secs_f64(args.working_timeout),
        max_attempts: args.max_attempts,
        deliver_while_working: false,
    };
    match command {
        "send" => {
            let result = agent::send(
                &client,
                &target,
                &queue,
                message.as_deref().expect("send message was loaded"),
                options,
            )?;
            write_json(&result)?;
            Ok(0)
        }
        "drain" => {
            let result = agent::drain(&client, &target, &queue, options)?;
            let code = if result.blocked.is_some() {
                75
            } else if result.quarantined.is_empty() {
                0
            } else {
                76
            };
            write_json(&result)?;
            Ok(code)
        }
        "status" => {
            write_json(&agent::status(&client, &target, &queue)?)?;
            Ok(0)
        }
        "read" => {
            print!("{}", agent::read(&client, &target, args.lines)?);
            Ok(0)
        }
        _ => unreachable!("parser restricted commands"),
    }
}

fn run_managed(args: Args) -> Result<i32, CliError> {
    let command = args.positional[0].as_str();
    if command != "stop"
        && (args.expected_token.is_some()
            || args.recover_legacy_adoption
            || args.retire_dead_adoption
            || args.expected_record_sha256.is_some())
    {
        return Err(CliError::Usage(
            "legacy recovery options are valid only with stop".to_owned(),
        ));
    }
    let positional = args.positional.get(1);
    let mut name = args.name.as_deref();
    if matches!(command, "start" | "stop") {
        if positional.is_some() && name.is_some() {
            return Err(CliError::Usage(
                "pass the agent name once, as a positional or --name".to_owned(),
            ));
        }
        name = name.or(positional.map(String::as_str));
    }
    if command != "list" && name.is_none_or(str::is_empty) {
        return Err(CliError::Usage(format!(
            "{command} needs a registered agent --name"
        )));
    }
    if args.pane.is_some()
        || args.session.is_some()
        || args.session_agent.is_some()
        || args.expected_agent.is_some()
        || args.expected_workspace.is_some()
    {
        return Err(CliError::Usage("managed commands use registry identity; do not combine --name with pane/session assertions".to_owned()));
    }
    if command == "stop" {
        if args.recover_legacy_adoption && args.retire_dead_adoption {
            return Err(CliError::Usage(
                "--retire-dead-adoption and --recover-legacy-adoption are mutually exclusive"
                    .to_owned(),
            ));
        }
        let recovery_selector = if args.retire_dead_adoption {
            Some("--retire-dead-adoption")
        } else if args.recover_legacy_adoption {
            Some("--recover-legacy-adoption")
        } else {
            None
        };
        if let Some(selector) = recovery_selector
            .filter(|_| args.expected_token.is_none() || args.expected_record_sha256.is_none())
        {
            return Err(CliError::Usage(format!(
                "{selector} requires --expected-token and --expected-record-sha256"
            )));
        }
        if recovery_selector.is_none() && args.expected_record_sha256.is_some() {
            return Err(CliError::Usage(
                "--expected-record-sha256 requires --recover-legacy-adoption or --retire-dead-adoption".to_owned(),
            ));
        }
    }
    let goal_command = args
        .goal_command_json
        .as_ref()
        .map(|raw| {
            let command: Vec<String> = serde_json::from_str(raw).map_err(|_| {
                CliError::Usage("goal command must be a nonempty JSON string array".to_owned())
            })?;
            if command.is_empty() || command.iter().any(String::is_empty) {
                return Err(CliError::Usage(
                    "goal command must be a nonempty JSON string array".to_owned(),
                ));
            }
            Ok(command)
        })
        .transpose()?;
    let client = HerdrClient::with_executable("direct", &args.herdr_bin)?;
    let manager = ManagedAgents::new(
        &client,
        &absolute_lexical(&args.registry).map_err(CliError::Output)?,
    )?;
    let delivery = DrainOptions {
        ready_timeout: Duration::from_secs_f64(args.ready_timeout),
        working_timeout: Duration::from_secs_f64(args.working_timeout),
        max_attempts: args.max_attempts,
        deliver_while_working: false,
    };
    let name = name.unwrap_or_default();
    match command {
        "start" => {
            let cwd = args
                .expected_cwd
                .as_deref()
                .ok_or_else(|| CliError::Usage("start needs --cwd".to_owned()))?;
            if args.file.is_some() && args.brief.is_some() {
                return Err(CliError::Usage(
                    "pass --brief or --file, not both".to_owned(),
                ));
            }
            let brief = match args.file {
                Some(ref path) => Some(fs::read_to_string(path).map_err(|error| {
                    CliError::Usage(format!("cannot read brief {}: {error}", path.display()))
                })?),
                None => args.brief.clone(),
            };
            let options = StartOptions {
                workspace_id: args.workspace_id.clone(),
                profile: None,
                harness: args.harness.clone(),
                model: args.model.clone(),
                resume: args.resume.clone(),
                harness_args: args.harness_args.clone(),
                environment: Vec::new(),
                brief,
                startup_timeout: Duration::from_secs_f64(args.startup_timeout),
                delivery,
                cloud: None,
                slot: None,
            };
            write_json(&manager.start(name, cwd, options)?)?;
        }
        "stop" => write_json(&manager.advised_stop_with_options(
            name,
            StopOptions {
                expected_token: args.expected_token.clone(),
                recover_legacy_adoption: args.recover_legacy_adoption,
                retire_dead_adoption: args.retire_dead_adoption,
                expected_record_sha256: args.expected_record_sha256.clone(),
                skip_cloud_halt: false,
            },
        )?)?,
        "list" => {
            if positional.is_some() || args.name.is_some() {
                return Err(CliError::Usage(
                    "list does not accept an agent name".to_owned(),
                ));
            }
            write_json(&manager.list()?)?;
        }
        "send" => write_json(&manager.send(name, &message(&args)?, delivery)?)?,
        "drain" => {
            let result = manager.drain(name, delivery)?;
            write_json(&result)?;
            return Ok(if result.blocked.is_some() {
                75
            } else if result.quarantined.is_empty() {
                0
            } else {
                76
            });
        }
        "status" => write_json(&manager.status(name)?)?,
        "read" => print!("{}", manager.read(name, args.lines)?),
        "wait" => write_json(&manager.wait(name, Duration::from_secs_f64(args.ready_timeout))?)?,
        "goal" => {
            let text = if positional.is_some() || args.file.is_some() {
                Some(message(&args)?)
            } else {
                None
            };
            write_json(&manager.goal(name, text.as_deref(), delivery, goal_command.as_deref())?)?;
        }
        "bind-session" => {
            if args.file.is_some() || positional.is_none() {
                return Err(CliError::Usage(
                    "bind-session needs an explicit session id as its positional argument"
                        .to_owned(),
                ));
            }
            write_json(&manager.bind_session(
                name,
                positional.expect("validated session"),
                goal_command.as_deref(),
            )?)?;
        }
        _ => {
            return Err(CliError::Usage(format!(
                "unsupported managed command: {command}"
            )))
        }
    }
    Ok(0)
}

fn print_stop_recovery(context: &StopContext, recovery: &RecoveryAction) {
    eprintln!("Recovery command: {}", context.command(recovery));
}

fn parsed_stop_context(args: &Args) -> Option<StopContext> {
    if args.positional.first().map(String::as_str) != Some("stop") {
        return None;
    }
    let registry = absolute_lexical(&args.registry).unwrap_or_else(|_| args.registry.clone());
    Some(StopContext::new(&registry, &args.herdr_bin))
}

fn emit_agent_error(
    error: &AgentError,
    stop_context: Option<&StopContext>,
    recovery: &RecoveryAction,
) -> i32 {
    if let Some(message) = error.undelivered() {
        let document = json!({
            "outcome": error.outcome().map(QueueOutcome::as_str),
            "message_id": message.message_id,
            "artifact": message.artifact,
            "error": error.to_string(),
            "safe_to_retry": error.safe_to_retry(),
        });
        if let Err(output_error) = write_json(&document) {
            eprintln!("herdr-agent: cannot write output: {output_error:?}");
            if let Some(context) = stop_context {
                print_stop_recovery(context, recovery);
            }
            return 1;
        }
    }
    if let Some(context) = stop_context {
        eprintln!("herdr-agent: {}", stop_reason(error));
        print_stop_recovery(context, recovery);
    } else if error.undelivered().is_none() {
        eprintln!("herdr-agent: {error}");
    }
    error.exit_code()
}

fn write_json(value: &impl Serialize) -> Result<(), CliError> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let canonical =
        serde_json::to_value(value).map_err(|error| CliError::Output(io::Error::other(error)))?;
    serde_json::to_writer_pretty(&mut output, &canonical)
        .map_err(|error| CliError::Output(io::Error::other(error)))?;
    output.write_all(b"\n").map_err(CliError::Output)
}

fn message(args: &Args) -> Result<String, CliError> {
    let text = args.positional.get(1);
    match (&args.file, text) {
        (Some(_), Some(_)) => Err(CliError::Usage(
            "pass message text or --file, not both".to_owned(),
        )),
        (Some(path), None) => fs::read_to_string(path).map_err(|error| {
            CliError::Usage(format!(
                "cannot read message artifact {}: {error}",
                path.display()
            ))
        }),
        (None, Some(text)) => Ok(text.clone()),
        (None, None) => Err(CliError::Usage(
            "send needs message text or --file".to_owned(),
        )),
    }
}

fn validate(args: &Args) -> Result<(), String> {
    validate_positionals(args)?;
    let command = args
        .positional
        .first()
        .ok_or_else(|| "a command is required".to_owned())?;
    debug_assert!(COMMANDS.contains(&command.as_str()));
    if !args.ready_timeout.is_finite()
        || !args.working_timeout.is_finite()
        || args.ready_timeout < 0.0
        || args.working_timeout <= 0.0
        || args.ready_timeout > MAX_WAIT_SECONDS
        || args.working_timeout > MAX_WAIT_SECONDS
        || !args.startup_timeout.is_finite()
        || args.startup_timeout <= 0.0
        || args.startup_timeout > 300.0
        || args.max_attempts == 0
        || args.lines == 0
    {
        return Err(format!(
            "timeouts must be finite and positive (ready-timeout may be zero and must not exceed {MAX_WAIT_SECONDS:.0}s); max-attempts and lines must be positive"
        ));
    }
    Ok(())
}

fn validate_positionals(args: &Args) -> Result<(), String> {
    if args.positional.len() > 2 {
        return Err(format!(
            "unrecognized arguments: {}",
            args.positional[2..].join(" ")
        ));
    }
    if let Some(command) = args.positional.first() {
        if !COMMANDS.contains(&command.as_str()) {
            return Err(format!("invalid command: {command:?}"));
        }
    }
    Ok(())
}

#[cfg(test)]
fn parse<I>(arguments: I) -> Result<ParseResult, String>
where
    I: IntoIterator<Item = OsString>,
{
    parse_with_context(arguments).map_err(|failure| failure.message)
}

fn parse_with_context<I>(arguments: I) -> Result<ParseResult, ParseFailure>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = Args::default();
    match parse_into(arguments, &mut args) {
        Ok(Some(code)) => Ok(ParseResult::Exit(code)),
        Ok(None) => Ok(ParseResult::Args(Box::new(args))),
        Err(message) => Err(ParseFailure {
            message,
            stop_context: parsed_stop_context(&args),
        }),
    }
}

fn parse_into<I>(arguments: I, args: &mut Args) -> Result<Option<i32>, String>
where
    I: IntoIterator<Item = OsString>,
{
    let raw = arguments
        .into_iter()
        .map(|value| {
            value
                .into_string()
                .map_err(|_| "arguments must be valid UTF-8".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut index = 0;
    let mut options = true;
    while index < raw.len() {
        let token = &raw[index];
        if options && token == "--" {
            options = false;
            index += 1;
            continue;
        }
        if options {
            match token.as_str() {
                "--help" | "-h" => {
                    print_help();
                    return Ok(Some(0));
                }
                "--version" => {
                    println!("herdr-agent {}", env!("CARGO_PKG_VERSION"));
                    return Ok(Some(0));
                }
                "--userguide" => args.userguide = true,
                "--recover-legacy-adoption" => args.recover_legacy_adoption = true,
                "--retire-dead-adoption" => args.retire_dead_adoption = true,
                option if value_option(option).is_some() => {
                    let value = raw
                        .get(index + 1)
                        .ok_or_else(|| format!("argument {option}: expected one value"))?
                        .clone();
                    if value.starts_with('-') && value != "-" {
                        return Err(format!("argument {option}: expected one value"));
                    }
                    assign(args, option, value)?;
                    index += 1;
                }
                option if option.starts_with("--") && option.contains('=') => {
                    let (name, value) = option.split_once('=').expect("guarded equals");
                    if value_option(name).is_none() {
                        return Err(format!("unrecognized arguments: {option}"));
                    }
                    assign(args, name, value.to_owned())?;
                }
                option if option.starts_with('-') => {
                    return Err(format!("unrecognized arguments: {option}"));
                }
                _ => args.positional.push(token.clone()),
            }
        } else {
            args.positional.push(token.clone());
        }
        index += 1;
    }
    Ok(None)
}

fn value_option(option: &str) -> Option<()> {
    matches!(
        option,
        "--file"
            | "--pane"
            | "--session-agent"
            | "--session"
            | "--agent"
            | "--workspace"
            | "--cwd"
            | "--queue"
            | "--ready-timeout"
            | "--working-timeout"
            | "--max-attempts"
            | "--lines"
            | "--herdr-bin"
            | "--name"
            | "--registry"
            | "--workspace-id"
            | "--harness"
            | "--model"
            | "--resume"
            | "--harness-arg"
            | "--brief"
            | "--startup-timeout"
            | "--goal-command-json"
            | "--expected-token"
            | "--expected-record-sha256"
    )
    .then_some(())
}

fn assign(args: &mut Args, option: &str, value: String) -> Result<(), String> {
    match option {
        "--file" => args.file = Some(PathBuf::from(value)),
        "--pane" => args.pane = Some(value),
        "--session-agent" => args.session_agent = Some(value),
        "--session" => args.session = Some(value),
        "--agent" => args.expected_agent = Some(value),
        "--workspace" => args.expected_workspace = Some(value),
        "--cwd" => args.expected_cwd = Some(PathBuf::from(value)),
        "--queue" => args.queue = PathBuf::from(value),
        "--ready-timeout" => args.ready_timeout = parse_float(option, &value)?,
        "--working-timeout" => args.working_timeout = parse_float(option, &value)?,
        "--max-attempts" => {
            args.max_attempts = parse_count(option, &value)?;
        }
        "--lines" => {
            args.lines = usize::try_from(parse_count(option, &value)?)
                .expect("MAX_COUNT fits every supported usize");
        }
        "--herdr-bin" => args.herdr_bin = PathBuf::from(value),
        "--name" => args.name = Some(value),
        "--registry" => args.registry = PathBuf::from(value),
        "--workspace-id" => args.workspace_id = Some(value),
        "--harness" => args.harness = value,
        "--model" => args.model = Some(value),
        "--resume" => args.resume = Some(value),
        "--harness-arg" => args.harness_args.push(value),
        "--brief" => args.brief = Some(value),
        "--startup-timeout" => args.startup_timeout = parse_float(option, &value)?,
        "--goal-command-json" => args.goal_command_json = Some(value),
        "--expected-token" => args.expected_token = Some(value),
        "--expected-record-sha256" => args.expected_record_sha256 = Some(value),
        _ => return Err(format!("unsupported option {option}")),
    }
    Ok(())
}

fn parse_float(option: &str, value: &str) -> Result<f64, String> {
    if !has_ascii_float_grammar(value) {
        return Err(format!(
            "argument {option}: invalid numeric value: {value:?}"
        ));
    }
    value
        .parse::<f64>()
        .map_err(|_| format!("argument {option}: invalid numeric value: {value:?}"))
}

fn has_ascii_float_grammar(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = usize::from(
        bytes
            .first()
            .is_some_and(|byte| matches!(byte, b'+' | b'-')),
    );
    let integer_start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    let mut digits = index - integer_start;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fractional_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        digits += index - fractional_start;
    }
    if digits == 0 {
        return false;
    }
    if bytes
        .get(index)
        .is_some_and(|byte| matches!(byte, b'e' | b'E'))
    {
        index += 1;
        if bytes
            .get(index)
            .is_some_and(|byte| matches!(byte, b'+' | b'-'))
        {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_start {
            return false;
        }
    }
    index == bytes.len()
}

fn parse_count(option: &str, value: &str) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "argument {option}: invalid integer value: {value:?}"
        ));
    }
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("argument {option}: invalid integer value: {value:?}"))?;
    if parsed > MAX_COUNT {
        return Err(format!(
            "argument {option}: value must not exceed {MAX_COUNT}"
        ));
    }
    Ok(parsed)
}

fn absolute_lexical(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn usage_line() -> &'static str {
    "herdr-agent [-h] [--version] [--userguide] [OPTIONS] [{start,stop,list,wait,goal,bind-session,send,drain,status,read,userguide}] [text]"
}

fn print_help() {
    println!(
        "usage: {}\n\nQueue, submit, inspect, and read interactive agents hosted in Herdr panes.\n\npositional arguments:\n  {{start,stop,list,wait,goal,bind-session,send,drain,status,read,userguide}}\n  text\n\noptions:\n  -h, --help\n  --version\n  --userguide\n  --file FILE\n  --pane PANE\n  --session-agent SESSION_AGENT\n  --session SESSION\n  --agent AGENT\n  --workspace WORKSPACE\n  --cwd CWD\n  --queue QUEUE\n  --ready-timeout READY_TIMEOUT\n  --working-timeout WORKING_TIMEOUT\n  --max-attempts MAX_ATTEMPTS\n  --lines LINES\n  --herdr-bin HERDR_BIN\n  --name NAME\n  --registry REGISTRY\n  --workspace-id WORKSPACE_ID\n  --harness HARNESS\n  --model MODEL\n  --resume SESSION\n  --harness-arg ARGUMENT\n  --brief TEXT\n  --startup-timeout SECONDS\n  --goal-command-json ARGV_JSON\n  --expected-token TOKEN\n  --recover-legacy-adoption\n  --retire-dead-adoption\n  --expected-record-sha256 SHA256\n\nAdoption recovery applies only to stop. Select exactly one recovery flag and supply\n--expected-token TOKEN and the lowercase 64-hex SHA-256 of the raw agent.json bytes.\n--retire-dead-adoption archives a running adopted record with a dead pinned harness\nwithout querying Herdr. Example: herdr-agent stop reviewer --retire-dead-adoption\n  --expected-token TOKEN --expected-record-sha256 SHA256",
        usage_line()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn stop_advice_parser_retains_only_its_recognized_command_and_parsed_globals() {
        let prefix = [
            "--registry=/work/legacy record dir/'registry'",
            "--herdr-bin=-literal herdr $argument",
        ];
        let expected = StopContext::new(
            Path::new("/work/legacy record dir/'registry'"),
            Path::new("-literal herdr $argument"),
        )
        .command(&RecoveryAction::Doctor);
        for suffix in [
            &["stop", "worker", "--unknown-option"][..],
            &["stop", "worker", "--expected-token"][..],
            &["stop", "worker", "--lines=bad-number"][..],
        ] {
            for arguments in [
                [&prefix[..], suffix].concat(),
                [&suffix[..1], &prefix[..], &suffix[1..]].concat(),
                [
                    &["--registry=/work/earlier", "--herdr-bin=earlier"][..],
                    &suffix[..1],
                    &prefix[..],
                    &suffix[1..],
                ]
                .concat(),
            ] {
                let failure = parse_with_context(strings(&arguments))
                    .err()
                    .expect("syntax refusal");
                assert!(!failure.message.is_empty());
                assert_eq!(
                    failure
                        .stop_context
                        .expect("grammar identified stop before the error")
                        .command(&RecoveryAction::Doctor),
                    expected
                );
            }
        }
        for arguments in [
            &["--registry", "stop", "--unknown-option"][..],
            &["send", "stop", "--unknown-option"][..],
            &["--unknown-option", "stop"][..],
        ] {
            let failure = parse_with_context(strings(arguments))
                .err()
                .expect("syntax refusal");
            assert!(failure.stop_context.is_none());
        }
        let ParseResult::Args(args) = parse_with_context(strings(&["stop"]))
            .ok()
            .expect("parsed command with missing name")
        else {
            panic!("unexpected early exit");
        };
        assert!(parsed_stop_context(&args).is_some());
    }

    #[test]
    fn stop_advice_bound_flags_accept_leading_hyphen_tokens_as_one_argument() {
        let ParseResult::Args(args) = parse(strings(&[
            "stop",
            "worker",
            "--expected-token=-original-generation",
        ]))
        .unwrap() else {
            panic!("unexpected early exit");
        };
        assert_eq!(args.expected_token.as_deref(), Some("-original-generation"));
    }

    #[test]
    fn dead_adoption_flags_preserve_authority_and_reject_invalid_modes_before_herdr() {
        let ParseResult::Args(mut args) = parse(strings(&[
            "stop",
            "foreign",
            "--retire-dead-adoption",
            "--expected-token=-original-generation",
            "--expected-record-sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ]))
        .unwrap() else {
            panic!("unexpected early exit");
        };
        assert!(args.retire_dead_adoption);
        assert!(!args.recover_legacy_adoption);
        assert_eq!(args.expected_token.as_deref(), Some("-original-generation"));
        assert_eq!(
            args.expected_record_sha256.as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        args.herdr_bin = PathBuf::from("/definitely/missing/herdr");
        args.recover_legacy_adoption = true;
        assert!(matches!(
            run(*args),
            Err(CliError::Usage(message)) if message.contains("mutually exclusive")
        ));

        let args = Args {
            positional: vec!["send".to_owned(), "text".to_owned()],
            pane: Some("p1".to_owned()),
            retire_dead_adoption: true,
            herdr_bin: PathBuf::from("/definitely/missing/herdr"),
            ..Args::default()
        };
        assert!(matches!(
            run(args),
            Err(CliError::Usage(message)) if message.contains("valid only with stop")
        ));
    }

    #[test]
    fn parser_accepts_intermixed_options_and_rejects_bad_domains() {
        let ParseResult::Args(args) = parse(strings(&[
            "send",
            "hello",
            "--pane",
            "p1",
            "--ready-timeout=0",
        ]))
        .unwrap() else {
            panic!("unexpected early exit");
        };
        assert_eq!(args.positional, ["send", "hello"]);
        assert_eq!(args.pane.as_deref(), Some("p1"));
        assert_eq!(args.ready_timeout, 0.0);
        assert!(validate(&args).is_ok());

        let ParseResult::Args(stop) = parse(strings(&[
            "stop",
            "legacy",
            "--recover-legacy-adoption",
            "--expected-token",
            "generation-1",
            "--expected-record-sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ]))
        .unwrap() else {
            panic!("unexpected early exit");
        };
        assert!(stop.recover_legacy_adoption);
        assert_eq!(stop.expected_token.as_deref(), Some("generation-1"));
        assert_eq!(
            stop.expected_record_sha256.as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );

        let mut invalid = args;
        invalid.working_timeout = f64::NAN;
        assert!(validate(&invalid).unwrap_err().contains("finite"));
    }

    #[test]
    fn parser_refuses_stolen_options_and_non_ascii_or_unbounded_numbers() {
        for values in [
            &["status", "--pane", "--help"][..],
            &["status", "--pane", "--version"][..],
            &["status", "--pane", "--queue", "state"][..],
            &["status", "--lines", "1_0"][..],
            &["status", "--lines", "١٢"][..],
            &["status", "--lines", "1000001"][..],
            &["status", "--ready-timeout", "1_0"][..],
            &["status", "--ready-timeout", "١.0"][..],
        ] {
            assert!(parse(strings(values)).is_err(), "accepted {values:?}");
        }
        let ParseResult::Args(args) =
            parse(strings(&["status", "--ready-timeout=.5", "--lines=00017"]))
                .expect("valid ASCII numeric forms")
        else {
            panic!("unexpected early exit");
        };
        assert_eq!(args.ready_timeout, 0.5);
        assert_eq!(args.lines, 17);
    }

    #[test]
    fn message_source_is_exactly_one_of_text_or_file() {
        let mut args = Args {
            positional: vec!["send".to_owned()],
            ..Args::default()
        };
        assert!(matches!(message(&args), Err(CliError::Usage(_))));
        args.positional.push("text".to_owned());
        args.file = Some(PathBuf::from("prompt.md"));
        assert!(matches!(message(&args), Err(CliError::Usage(_))));
    }

    #[test]
    fn empty_target_is_rejected_before_herdr_executable_resolution() {
        let args = Args {
            positional: vec!["status".to_owned()],
            pane: Some(String::new()),
            herdr_bin: PathBuf::from("/definitely/missing/herdr"),
            ..Args::default()
        };
        let error = run(args).unwrap_err();
        let CliError::Agent(error) = error else {
            panic!("expected target validation error, got {error:?}");
        };
        assert_eq!(error.exit_code(), 75);
        assert_eq!(
            error.to_string(),
            "target needs --pane or a stable session value"
        );
    }
}
