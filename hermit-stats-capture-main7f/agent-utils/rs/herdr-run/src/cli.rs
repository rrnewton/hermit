//! Command-line interface for `herdr-run`.
//!
//! The surface is a plain multiplexed CLI: `herdr-run [GLOBAL OPTIONS] <subcommand> [OPTIONS]`.
//! Options that identify the invocation and its configuration are global and go before the
//! subcommand; options that only mean something to one subcommand go after it. Neither level
//! documents nor accepts the other's options, so `--help` at each level describes exactly the
//! options that level takes.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::allowlist::{admit, quote_word, Admission};
use crate::audit;
use crate::client::{bounded_output, exit_status_code, HerdrClient};
use crate::config::{load_config, missing_defaults_warning, Config, MAX_TIMEOUT_SECONDS};
use crate::error::{ErrorKind, HerdrRunError, Result, EXIT_BUSY};
use crate::evict;
use crate::output;
use crate::readiness::{assess, infer_prompt_tail};
use crate::reap::plan_reap;
use crate::runner::{execute, result_json, write_meta, RunResult};
use crate::session::{resolve_target, tab_label_for};
use crate::sweep::{build_evidence, load_run_records, measure_occupancy};
use crate::textfmt::{
    display, json_pretty, name_violation, os_error_text, quote, strip_space, whole_milliseconds,
};

/// One subcommand and the single line describing it in `herdr-run --help`.
struct Subcommand {
    name: &'static str,
    summary: &'static str,
}

/// Every subcommand, in the order `herdr-run --help` lists them.
///
/// Listed by what a reader is most likely to want first rather than alphabetically: the action,
/// then the two questions asked about it, then setup, then the reports.
const SUBCOMMANDS: &[Subcommand] = &[
    Subcommand {
        name: "run",
        summary: "run one allowlisted command in a pane and return its result",
    },
    Subcommand {
        name: "check",
        summary: "say whether a command would be admitted; touch no pane",
    },
    Subcommand {
        name: "status",
        summary: "report the configuration, policy, and session in effect; change nothing",
    },
    Subcommand {
        name: "init",
        summary: "write an annotated .herdr-run.yaml in this directory",
    },
    Subcommand {
        name: "config",
        summary: "print the fully resolved configuration as JSON",
    },
    Subcommand {
        name: "target",
        summary: "resolve this agent's pane and print its ids and readiness",
    },
    Subcommand {
        name: "reap",
        summary: "report which command tabs are provably finished with",
    },
    Subcommand {
        name: "net-doctor",
        summary: "smoke-test one scenario: a caller whose own network is blocked",
    },
    Subcommand {
        name: "quickstart",
        summary: "print the one-screen introduction",
    },
    Subcommand {
        name: "userguide",
        summary: "print the complete reference",
    },
];

/// Options accepted only before the subcommand.
#[derive(Clone, Debug, Default, PartialEq)]
struct Globals {
    config: Option<PathBuf>,
    agent: Option<String>,
    json: bool,
}

/// Options accepted only after `run`.
#[derive(Clone, Debug, Default, PartialEq)]
struct RunOptions {
    command: String,
    cwd: Option<String>,
    timeout: Option<f64>,
    wait_ready: Option<f64>,
    no_cache: bool,
    dry_run: bool,
}

/// The selected subcommand together with its own options.
#[derive(Clone, Debug, PartialEq)]
enum Selected {
    Run(RunOptions),
    Check(String),
    Status,
    Init { force: bool, upgrade: bool },
    Config,
    Target { no_cache: bool },
    Reap,
    NetDoctor,
    Quickstart,
    Userguide,
}

/// One fully parsed invocation.
#[derive(Clone, Debug, PartialEq)]
struct Invocation {
    globals: Globals,
    selected: Selected,
}

enum ParseResult {
    Invocation(Box<Invocation>),
    Exit(i32),
}

/// Why a command line was not turned into an invocation.
enum ParseFailure {
    /// A command line this surface cannot accept, carrying the message to print.
    Usage(String),
    /// Help or version output that could not be written.
    Failed(HerdrRunError),
}

impl From<String> for ParseFailure {
    fn from(message: String) -> Self {
        Self::Usage(message)
    }
}

impl From<HerdrRunError> for ParseFailure {
    fn from(error: HerdrRunError) -> Self {
        Self::Failed(error)
    }
}

type Parsed<T> = std::result::Result<T, ParseFailure>;

/// Make this process behave as the other edition's runtime does, before anything runs.
///
/// Called by the executable before [`main`]; tests that drive [`main`] in-process do not call it.
///
/// * SIGXFSZ is ignored, as the other edition's runtime ignores it: a write past `RLIMIT_FSIZE`
///   then fails with EFBIG and is reported as `cannot write stdout: File too large (os error 27)`
///   instead of killing the process. Children get the default action back in
///   [`crate::client::bounded_output`].
/// * Every descriptor above 2 inherited from the caller is marked close-on-exec, so no child
///   inherits it, just as the other edition's children (started with `close_fds`) do not. Every
///   descriptor this process opens itself is close-on-exec already.
///
/// * SIGCHLD gets its default action. A caller can hand an ignored SIGCHLD down across `exec`,
///   and then the kernel reaps every child at once: waiting for one fails with ECHILD, so no exit
///   status can be read. The other edition's runtime would read that as status 0 -- every failed
///   child a success -- and this one as an error; with the default action restored both read the
///   real status, and so do the processes either edition starts.
///
/// * Which signals the caller left ignored is recorded, before anything changes a disposition and
///   before any thread exists, so every child gets them ignored again (see
///   [`crate::client::bounded_output`]): the C library takes signal 33 for itself when the first
///   thread starts, and the other edition, which starts no thread, hands it on as it found it.
///
/// SIGINT is left as it was inherited, and closed standard descriptors were already reopened on
/// `/dev/null` (inheritably) by the runtime before `main`.
pub fn prepare_process() {
    crate::client::record_inherited_ignored_signals();
    // SAFETY: `signal` with a constant disposition has no pointer arguments.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
    }
    mark_inherited_descriptors_close_on_exec();
}

fn mark_inherited_descriptors_close_on_exec() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `close_range` takes plain integers; with CLOSE_RANGE_CLOEXEC it closes nothing.
        let marked = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        } == 0;
        if marked {
            return;
        }
    }
    // Kernels before 5.11: list the descriptors, then mark each one.
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return;
    };
    let descriptors: Vec<libc::c_int> = entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter(|descriptor| *descriptor > 2)
        .collect();
    for descriptor in descriptors {
        // SAFETY: `fcntl` on a descriptor number; one that has closed meanwhile returns EBADF.
        unsafe {
            let flags = libc::fcntl(descriptor, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// Run the CLI over an argument iterator and return the desired process status.
pub fn main<I>(arguments: I) -> i32
where
    I: IntoIterator<Item = OsString>,
{
    let parsed = match parse(arguments) {
        Ok(parsed) => parsed,
        Err(ParseFailure::Usage(message)) => {
            output::diagnostic(&format!("usage: {}", usage_line()));
            output::diagnostic(&format!("herdr-run: error: {message}"));
            return 2;
        }
        Err(ParseFailure::Failed(error)) => {
            output::diagnostic(&format!("herdr-run: {error}"));
            return error.exit_code();
        }
    };
    let invocation = match parsed {
        ParseResult::Invocation(invocation) => *invocation,
        ParseResult::Exit(code) => return code,
    };
    // Printed before any configuration is read: documentation is exactly what somebody with a
    // broken configuration file needs, and refusing to show it would be perverse.
    let outcome = match document_for(&invocation.selected) {
        Some(document) => output::stdout(document).map(|()| 0),
        None => dispatch(invocation),
    };
    match outcome {
        Ok(code) => code,
        Err(error) => {
            output::diagnostic(&format!("herdr-run: {error}"));
            error.exit_code()
        }
    }
}

/// Return the document a documentation subcommand prints, or `None` for the working subcommands.
fn document_for(selected: &Selected) -> Option<&'static str> {
    match selected {
        Selected::Quickstart => Some(crate::QUICKSTART),
        Selected::Userguide => Some(crate::USER_GUIDE),
        _ => None,
    }
}

fn find_subcommand(name: &str) -> Option<&'static Subcommand> {
    SUBCOMMANDS.iter().find(|entry| entry.name == name)
}

fn subcommand_names() -> String {
    let mut names: Vec<&str> = SUBCOMMANDS.iter().map(|entry| entry.name).collect();
    names.sort_unstable();
    names.join(", ")
}

/// Return the subcommands that own `name`, or an empty slice if no subcommand does.
///
/// Knowing the owner is what turns "unrecognized argument" into a usable message. A caller who
/// writes `herdr-run --cwd /tmp run ...` has the right option and the wrong level, and being told
/// which level it belongs to is the whole difference between a typo and a dead end.
fn local_option_owners(name: &str) -> Vec<&'static str> {
    if name == "--help" || name == "-h" {
        return Vec::new();
    }
    SUBCOMMAND_OPTIONS
        .iter()
        .filter(|(_, table)| table.iter().any(|(option, _)| *option == name))
        .map(|(subcommand, _)| *subcommand)
        .collect()
}

/// Every option accepted before the subcommand, with the value it takes (`None` for a flag).
///
/// The parser, the refusal messages, and `--dump-surface` all read this one table, so the surface
/// the tool reports is the surface it accepts.
const GLOBAL_OPTIONS: &[(&str, Option<&str>)] = &[
    ("--agent", Some("NAME")),
    ("--config", Some("PATH")),
    ("--dump-surface", None),
    ("--help", None),
    ("--json", None),
    ("--version", None),
    ("-h", None),
];

/// One option table: each option and the value it takes (`None` for a flag).
type OptionTable = &'static [(&'static str, Option<&'static str>)];

/// Every option each subcommand accepts after its name, with the value it takes.
const SUBCOMMAND_OPTIONS: &[(&str, OptionTable)] = &[
    ("check", &[("--help", None), ("-h", None)]),
    ("config", &[("--help", None), ("-h", None)]),
    (
        "init",
        &[
            ("--force", None),
            ("--help", None),
            ("--upgrade", None),
            ("-h", None),
        ],
    ),
    ("net-doctor", &[("--help", None), ("-h", None)]),
    ("quickstart", &[("--help", None), ("-h", None)]),
    ("reap", &[("--help", None), ("-h", None)]),
    (
        "run",
        &[
            ("--cwd", Some("PATH")),
            ("--dry-run", None),
            ("--help", None),
            ("--no-cache", None),
            ("--timeout", Some("SECONDS")),
            ("--wait-ready", Some("SECONDS")),
            ("-h", None),
        ],
    ),
    ("status", &[("--help", None), ("-h", None)]),
    (
        "target",
        &[("--help", None), ("--no-cache", None), ("-h", None)],
    ),
    ("userguide", &[("--help", None), ("-h", None)]),
];

/// The positional argument each subcommand takes, if any.
fn positional_for(subcommand: &str) -> &'static [&'static str] {
    match subcommand {
        "run" | "check" => &["<command>"],
        _ => &[],
    }
}

fn is_global_option(name: &str) -> bool {
    GLOBAL_OPTIONS.iter().any(|(option, _)| *option == name)
}

/// The JSON document `--dump-surface` prints: every subcommand and option, from the tables above.
fn surface_document() -> Value {
    let options = |table: &[(&str, Option<&str>)]| {
        let mut map = Map::new();
        for (name, value) in table {
            map.insert((*name).to_owned(), json!(value));
        }
        Value::Object(map)
    };
    let mut subcommands = Map::new();
    for (name, table) in SUBCOMMAND_OPTIONS {
        subcommands.insert(
            (*name).to_owned(),
            json!({"options": options(table), "positional": positional_for(name)}),
        );
    }
    json!({
        "global_options": options(GLOBAL_OPTIONS),
        "subcommands": Value::Object(subcommands),
        "version": env!("CARGO_PKG_VERSION"),
    })
}

fn join_quoted(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!("'{value}'"))
        .collect::<Vec<_>>()
        .join(" or ")
}

/// Split `--name=value` into its parts; a bare `--name` yields no inline value.
fn split_option(token: &str) -> (&str, Option<&str>) {
    match token.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (token, None),
    }
}

fn option_value(
    raw: &[String],
    index: &mut usize,
    name: &str,
    inline: Option<&str>,
) -> Parsed<String> {
    if let Some(value) = inline {
        return Ok(value.to_owned());
    }
    let Some(value) = raw.get(*index + 1) else {
        return Err(format!("argument {name}: expected one value").into());
    };
    if value.starts_with('-') && value != "-" {
        return Err(format!("argument {name}: expected one value").into());
    }
    *index += 1;
    Ok(value.clone())
}

/// The seconds grammar `--timeout` and `--wait-ready` accept: ASCII digits only, an optional sign,
/// an optional fraction and exponent, or a spelling of infinity or NaN (refused afterwards as not
/// finite). No whitespace, no underscores, no other script's digits.
fn seconds_grammar() -> &'static Regex {
    static GRAMMAR: OnceLock<Regex> = OnceLock::new();
    GRAMMAR.get_or_init(|| {
        Regex::new(
            r"^[+-]?(?:(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?|[iI][nN][fF](?:[iI][nN][iI][tT][yY])?|[nN][aA][nN])$",
        )
        .expect("valid seconds grammar")
    })
}

fn parse_seconds(option: &str, value: &str) -> Parsed<f64> {
    if !seconds_grammar().is_match(value) {
        return Err(format!("argument {option}: invalid numeric value: {}", quote(value)).into());
    }
    let parsed = value
        .parse::<f64>()
        .map_err(|_| format!("argument {option}: invalid numeric value: {}", quote(value)))?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err(format!("argument {option}: must be a finite non-negative number").into());
    }
    if parsed > MAX_TIMEOUT_SECONDS {
        return Err(
            format!("argument {option}: must not exceed {MAX_TIMEOUT_SECONDS:.0} seconds").into(),
        );
    }
    if !whole_milliseconds(parsed) {
        return Err(format!("argument {option}: must be a whole number of milliseconds").into());
    }
    Ok(parsed)
}

fn parse<I>(arguments: I) -> Parsed<ParseResult>
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
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut globals = Globals::default();
    let mut index = 0;
    let mut chosen: Option<&'static Subcommand> = None;
    while index < raw.len() {
        let token = raw[index].clone();
        if !token.starts_with('-') {
            match find_subcommand(&token) {
                Some(subcommand) => {
                    chosen = Some(subcommand);
                    index += 1;
                    break;
                }
                None => return Err(unknown_subcommand(&token, &raw[index + 1..]).into()),
            }
        }
        if token == "--" {
            return Err(format!(
                "expected a subcommand before '--'. Subcommands: {}",
                subcommand_names()
            )
            .into());
        }
        match token.as_str() {
            "--help" | "-h" => {
                print_help()?;
                return Ok(ParseResult::Exit(0));
            }
            "--version" => {
                output::stdout(&format!("herdr-run {}\n", env!("CARGO_PKG_VERSION")))?;
                return Ok(ParseResult::Exit(0));
            }
            "--dump-surface" => {
                output::stdout(&format!("{}\n", json_pretty(&surface_document())))?;
                return Ok(ParseResult::Exit(0));
            }
            "--json" => globals.json = true,
            _ => {
                let (name, inline) = split_option(&token);
                match name {
                    "--config" => {
                        globals.config =
                            Some(PathBuf::from(option_value(&raw, &mut index, name, inline)?));
                    }
                    "--agent" => {
                        globals.agent = Some(option_value(&raw, &mut index, name, inline)?);
                    }
                    "--json" | "--help" | "--version" | "--dump-surface" => {
                        return Err(format!("argument {name}: takes no value").into());
                    }
                    other => return Err(misplaced_global(other).into()),
                }
            }
        }
        index += 1;
    }

    let Some(subcommand) = chosen else {
        // No subcommand at all is not an error: it is somebody asking what this command is.
        print_help()?;
        return Ok(ParseResult::Exit(0));
    };
    let rest = &raw[index..];
    let selected = match parse_subcommand(subcommand.name, rest)? {
        Some(selected) => selected,
        None => return Ok(ParseResult::Exit(0)),
    };
    // After the subcommand's own parse, so that `--config P init --help` still prints init's help
    // and `--config P init --nonsense` still names the option it could not accept. Checked in the
    // order the global option block lists them, so a command line that offers two blind globals at
    // once reports the first of them and both editions report the same one.
    if globals.config.is_some() {
        if let Some(reason) = config_blind_reason(subcommand.name) {
            return Err(config_blind_error(subcommand.name, reason).into());
        }
    }
    if globals.agent.is_some() {
        if let Some(reason) = agent_blind_reason(subcommand.name) {
            return Err(agent_blind_error(subcommand.name, reason).into());
        }
    }
    if globals.json {
        if let Some(reason) = json_blind_reason(subcommand.name) {
            return Err(json_blind_error(subcommand.name, reason).into());
        }
    }
    Ok(ParseResult::Invocation(Box::new(Invocation {
        globals,
        selected,
    })))
}

/// Parse one subcommand's own arguments; `None` means its `--help` was printed.
fn parse_subcommand(name: &str, rest: &[String]) -> Parsed<Option<Selected>> {
    match name {
        "run" => parse_run(rest),
        "check" => {
            let Some(positional) = parse_bare(name, rest)? else {
                return Ok(None);
            };
            match positional.len() {
                0 => Err("check: needs a command to check".to_owned().into()),
                1 => Ok(Some(Selected::Check(positional[0].clone()))),
                _ => Err(one_argument_error("check", &positional).into()),
            }
        }
        "status" | "config" | "reap" | "net-doctor" | "quickstart" | "userguide" => {
            let Some(positional) = parse_bare(name, rest)? else {
                return Ok(None);
            };
            if !positional.is_empty() {
                return Err(format!(
                    "{name}: takes no positional arguments; got {}",
                    positional.len()
                )
                .into());
            }
            Ok(Some(match name {
                "status" => Selected::Status,
                "config" => Selected::Config,
                "reap" => Selected::Reap,
                "net-doctor" => Selected::NetDoctor,
                "quickstart" => Selected::Quickstart,
                _ => Selected::Userguide,
            }))
        }
        "init" => {
            let (mut force, mut upgrade) = (false, false);
            for token in rest {
                if token == "--help" || token == "-h" {
                    print_subcommand_help(name)?;
                    return Ok(None);
                }
                match token.as_str() {
                    "--force" => force = true,
                    "--upgrade" => upgrade = true,
                    _ => return Err(local_argument_error(name, token).into()),
                }
            }
            if force && upgrade {
                return Err("init: --force and --upgrade cannot be combined: --force writes a fresh file over the existing one, and --upgrade edits the existing one in place".to_owned().into());
            }
            Ok(Some(Selected::Init { force, upgrade }))
        }
        "target" => {
            let mut present = false;
            for token in rest {
                if token == "--help" || token == "-h" {
                    print_subcommand_help(name)?;
                    return Ok(None);
                }
                if token == "--no-cache" {
                    present = true;
                    continue;
                }
                return Err(local_argument_error(name, token).into());
            }
            Ok(Some(Selected::Target { no_cache: present }))
        }
        _ => Err(format!("unknown subcommand '{name}'").into()),
    }
}

/// Parse a subcommand that takes no options, returning its positional arguments.
fn parse_bare(name: &str, rest: &[String]) -> Parsed<Option<Vec<String>>> {
    let mut positional = Vec::new();
    let mut options = true;
    for token in rest {
        if options && token == "--" {
            options = false;
            continue;
        }
        if options && (token == "--help" || token == "-h") {
            print_subcommand_help(name)?;
            return Ok(None);
        }
        if options && token.starts_with('-') {
            return Err(local_argument_error(name, token).into());
        }
        positional.push(token.clone());
    }
    Ok(Some(positional))
}

fn parse_run(rest: &[String]) -> Parsed<Option<Selected>> {
    let mut options = RunOptions::default();
    let mut positional = Vec::new();
    let mut parsing = true;
    let mut index = 0;
    while index < rest.len() {
        let token = rest[index].clone();
        if parsing && token == "--" {
            parsing = false;
            index += 1;
            continue;
        }
        if !parsing || !token.starts_with('-') {
            positional.push(token);
            index += 1;
            continue;
        }
        match token.as_str() {
            "--help" | "-h" => {
                print_subcommand_help("run")?;
                return Ok(None);
            }
            "--no-cache" => options.no_cache = true,
            "--dry-run" => options.dry_run = true,
            _ => {
                let (name, inline) = split_option(&token);
                match name {
                    "--cwd" => {
                        options.cwd = Some(option_value(rest, &mut index, name, inline)?);
                    }
                    "--timeout" => {
                        let value = option_value(rest, &mut index, name, inline)?;
                        options.timeout = Some(parse_seconds(name, &value)?);
                    }
                    "--wait-ready" => {
                        let value = option_value(rest, &mut index, name, inline)?;
                        options.wait_ready = Some(parse_seconds(name, &value)?);
                    }
                    other => return Err(local_argument_error("run", other).into()),
                }
            }
        }
        index += 1;
    }
    match positional.len() {
        0 => Err("run: needs a command to run".to_owned().into()),
        1 => {
            options.command = positional[0].clone();
            Ok(Some(Selected::Run(options)))
        }
        _ => Err(one_argument_error("run", &positional).into()),
    }
}

fn one_argument_error(subcommand: &str, positional: &[String]) -> String {
    let joined = positional.join(" ");
    format!(
        "{subcommand}: pass the command as ONE quoted argument, not as separate words.\n  you wrote:  herdr-run {subcommand} {joined}\n  instead:    herdr-run {subcommand} '{joined}'\nRe-joining loose words would silently change the quoting of your arguments."
    )
}

/// Describe an option that appeared after the subcommand but does not belong to it.
fn local_argument_error(subcommand: &str, token: &str) -> String {
    let (name, _) = split_option(token);
    if is_global_option(name) && name != "--help" && name != "-h" {
        return format!(
            "argument {name}: this is a GLOBAL option; put it before the subcommand:\n  herdr-run {name} ... {subcommand} ..."
        );
    }
    let owners = local_option_owners(name);
    if !owners.is_empty() && !owners.contains(&subcommand) {
        return format!(
            "argument {name}: this is a {} option, not a '{subcommand}' option",
            join_quoted(&owners)
        );
    }
    format!("{subcommand}: unrecognized arguments: {token}")
}

/// Why a subcommand takes no configuration file, or `None` when it takes one.
///
/// `--config` is refused for these rather than accepted and ignored. It is not an inert flag: it
/// reads as an instruction about which configuration file to use, and `herdr-run --config P init`
/// reads as an instruction to write `P`. Accepting an instruction and then disregarding it is the
/// defect this surface exists to remove, so the answer is a refusal that says why.
fn config_blind_reason(subcommand: &str) -> Option<&'static str> {
    match subcommand {
        "init" => {
            Some("it WRITES one into the current directory, or with --upgrade edits the one there")
        }
        "quickstart" | "userguide" => Some("it prints a packaged document"),
        _ => None,
    }
}

/// Describe `--config` offered to a subcommand that takes no configuration file.
fn config_blind_error(subcommand: &str, reason: &str) -> String {
    let message =
        format!("argument --config: '{subcommand}' takes no configuration file; {reason}.");
    if subcommand == "init" {
        return format!(
            "{message}\nRun it from the directory you want the file in:\n  cd DIRECTORY && herdr-run {subcommand}"
        );
    }
    format!("{message}\nDrop --config.")
}

/// Why a subcommand names no tab, or `None` when it names one.
///
/// `--agent NAME` says which agent this invocation speaks for, and the one thing that does is name
/// the tab the work happens in. A subcommand that resolves no tab therefore cannot observe it, and
/// a caller who wrote it was saying "do this as NAME" — an instruction, not a decoration. It is
/// refused for the same reason `--config` is: accepting an instruction and then disregarding it is
/// the defect this surface exists to remove.
fn agent_blind_reason(subcommand: &str) -> Option<&'static str> {
    match subcommand {
        "check" => Some("it answers a policy question about a command and touches no pane"),
        "init" => Some("it writes a configuration file and touches no pane"),
        "reap" => Some("it reports on the whole workspace, not on one agent's tab"),
        "quickstart" | "userguide" => Some("it prints a packaged document"),
        _ => None,
    }
}

/// Describe `--agent` offered to a subcommand that names no tab.
fn agent_blind_error(subcommand: &str, reason: &str) -> String {
    format!("argument --agent: '{subcommand}' names no tab; {reason}.\nDrop --agent.")
}

/// Why a subcommand has no machine-readable output, or `None` when it has one.
///
/// `--json` promises machine-readable output *where a subcommand has it*. Where one does not, the
/// promise cannot be kept: the flag is accepted, prose comes out, and the caller finds out at the
/// parse of the output rather than at the flag — which is the worst place to find out, because the
/// failure looks like malformed data rather than a rejected request.
///
/// `config`, `target`, and `reap` are deliberately NOT here. They print JSON with or without the
/// flag, so a caller who asks them for machine-readable output gets exactly that; the flag is
/// redundant there, not disregarded.
fn json_blind_reason(subcommand: &str) -> Option<&'static str> {
    match subcommand {
        "net-doctor" => Some("it prints a human diagnosis and a verdict, in prose"),
        "quickstart" | "userguide" => Some("it prints a packaged document"),
        _ => None,
    }
}

/// Describe `--json` offered to a subcommand with no machine-readable output.
fn json_blind_error(subcommand: &str, reason: &str) -> String {
    format!(
        "argument --json: '{subcommand}' has no machine-readable output; {reason}.\nDrop --json."
    )
}

/// Describe an option that appeared before the subcommand but belongs to one of them.
fn misplaced_global(name: &str) -> String {
    let owners = local_option_owners(name);
    if owners.is_empty() {
        return format!("unrecognized arguments: {name}");
    }
    format!(
        "argument {name}: this is a {} option; put it AFTER the subcommand:\n  herdr-run {} {name} ...",
        join_quoted(&owners),
        owners[0]
    )
}

/// Describe a first positional that is not a subcommand, naming the replacement for the old form.
///
/// `herdr-run <agent> '<command>'` and `herdr-run '<command>'` used to run a command with no
/// subcommand at all. Removing that is the point of this surface, so the removal has to be said
/// out loud, with the exact line to type instead — an "unknown subcommand" alone would leave a
/// caller guessing that the tool had been withdrawn.
fn unknown_subcommand(token: &str, following: &[String]) -> String {
    let mut message = format!("unknown subcommand '{token}'");
    let replacement = if following.len() == 1 && !following[0].starts_with('-') {
        Some(format!("herdr-run --agent {token} run '{}'", following[0]))
    } else if following.is_empty() && token.contains(' ') {
        Some(format!("herdr-run run '{token}'"))
    } else {
        None
    };
    if let Some(replacement) = replacement {
        message.push_str(
            "\nRunning a command without a subcommand is no longer accepted. Use 'run':\n    ",
        );
        message.push_str(&replacement);
    }
    message.push_str(&format!("\nSubcommands: {}", subcommand_names()));
    message
}

fn dispatch(invocation: Invocation) -> Result<i32> {
    let start = std::env::current_dir().map_err(|error| {
        HerdrRunError::config(format!(
            "cannot determine current directory: {}",
            os_error_text(&error)
        ))
    })?;
    if let Selected::Init { force, upgrade } = invocation.selected {
        // Deliberately before load_config: the reason to reach for `init` is often that discovery
        // found nothing, or found something broken, and refusing to write a fresh template because
        // the old one will not parse would be exactly the wrong moment to be strict.
        if upgrade {
            return command_init_upgrade(&start, invocation.globals.json);
        }
        return command_init(&start, force, invocation.globals.json);
    }
    let config = load_config(invocation.globals.config.as_deref(), &start)?;
    // Every subcommand that reads the configuration says so when a deny list in it leaves out a
    // built-in entry: the list replaces the built-in one, so the entry is not enforced, and a file
    // written by an older release would otherwise lose each new entry without a word.
    if let Some(warning) = missing_defaults_warning(&config) {
        output::diagnostic(&warning);
    }
    let json = invocation.globals.json;
    // The agent is resolved only by the subcommands that name a tab with it, so a malformed agent
    // variable cannot fail a `check` or a `reap` that never asks who is speaking.
    match invocation.selected {
        Selected::Run(options) => {
            let agent = resolve_agent(&invocation.globals)?;
            run_command(&config, &agent, json, &options)
        }
        Selected::Check(command) => command_check(&config, &command, json),
        Selected::Status => command_status(&config, &resolve_agent(&invocation.globals)?, json),
        Selected::Init { .. } => unreachable!("init is handled before configuration is loaded"),
        Selected::Config => command_config(&config, &resolve_agent(&invocation.globals)?),
        Selected::Target { no_cache } => {
            command_target(&config, &resolve_agent(&invocation.globals)?, !no_cache)
        }
        Selected::Reap => command_reap(&config),
        Selected::NetDoctor => command_net_doctor(&config, &resolve_agent(&invocation.globals)?),
        Selected::Quickstart | Selected::Userguide => Ok(0),
    }
}

/// The agent this invocation speaks for: `--agent`, else [`default_agent`].
///
/// An agent name, from either source, may hold no character a name may not contain
/// ([`name_violation`]): no control, no invisible character, no noncharacter, no private-use
/// character, and no unassigned code point. It names a tab somebody will read, and is printed.
fn resolve_agent(globals: &Globals) -> Result<String> {
    resolve_agent_from(globals.agent.as_deref(), |key| std::env::var_os(key))
}

/// [`resolve_agent`] over an explicit `--agent` value and environment lookup.
fn resolve_agent_from<F>(flag: Option<&str>, lookup: F) -> Result<String>
where
    F: Fn(&str) -> Option<OsString>,
{
    if let Some(agent) = flag.filter(|value| !value.is_empty()) {
        if let Some(violation) = name_violation(agent) {
            return Err(HerdrRunError::config(format!(
                "argument --agent: an agent name may not contain {violation}"
            )));
        }
        return Ok(agent.to_owned());
    }
    default_agent(lookup)
}

/// Infer the invoking agent from the environment the orchestrator sets.
///
/// The first of the three variables that is set and non-empty once Unicode `White_Space` is
/// trimmed from both ends (the explicit table in [`crate::textfmt`], so a value of only U+3000 or
/// U+00A0 counts as empty and the next variable is consulted). A value that still holds a
/// character a name may not contain after trimming is an error rather than a tab label. A
/// variable that is not valid UTF-8 is an error too: skipping it would make this invocation speak
/// for whoever the NEXT variable names.
fn default_agent<F>(lookup: F) -> Result<String>
where
    F: Fn(&str) -> Option<OsString>,
{
    for key in ["HERDR_RUN_AGENT", "DG_AGENT_NAME", "ORC_AGENT_NAME"] {
        let Some(raw) = lookup(key) else {
            continue;
        };
        let Ok(value) = raw.into_string() else {
            return Err(HerdrRunError::config(format!(
                "environment variable {key} is not valid UTF-8"
            )));
        };
        let trimmed = strip_space(&value);
        if trimmed.is_empty() {
            continue;
        }
        if let Some(violation) = name_violation(trimmed) {
            return Err(HerdrRunError::config(format!(
                "environment variable {key}: an agent name may not contain {violation}"
            )));
        }
        return Ok(trimmed.to_owned());
    }
    Ok("unknown-agent".to_owned())
}

fn command_init(directory: &Path, force: bool, json_output: bool) -> Result<i32> {
    let path = crate::init::write_config_template(directory, force)?;
    if json_output {
        print_json(&json!({"created": true, "path": path.display().to_string()}))?;
    } else {
        output::stdout(&format!(
            "wrote {}\nEvery knob is in that file, set to the value in force today. The allowlist near the\ntop is a HUMAN-ONLY knob: an agent that can widen its own allowlist does not have one.\n",
            display(&path.to_string_lossy())
        ))?;
    }
    Ok(0)
}

/// Upgrade the configuration file in `directory` and say what changed and what did not.
fn command_init_upgrade(directory: &Path, json_output: bool) -> Result<i32> {
    let result = crate::init::upgrade_config_file(directory)?;
    let quoted = |entries: &[&str]| {
        entries
            .iter()
            .map(|entry| quote(entry))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if json_output {
        print_json(&json!({
            "missing": result.missing.iter().map(|item| json!({
                "entries": item.entries,
                "setting": item.setting,
            })).collect::<Vec<_>>(),
            "path": result.path.display().to_string(),
            "upgraded": result.upgraded.iter().map(|upgrade| json!({
                "added": upgrade.added,
                "setting": upgrade.setting,
            })).collect::<Vec<_>>(),
        }))?;
        return Ok(0);
    }
    let shown = display(&result.path.to_string_lossy());
    let mut lines = Vec::new();
    if result.upgraded.is_empty() {
        lines.push(format!("nothing to rewrite in {shown}"));
    } else {
        lines.push(format!("upgraded {shown}"));
        for upgrade in &result.upgraded {
            lines.push(format!(
                "  {}: added {}",
                upgrade.setting,
                quoted(upgrade.added)
            ));
        }
    }
    if result.missing.is_empty() {
        lines.push("Every built-in deny entry is in force.".to_owned());
    } else {
        lines.push(
            "Still left out, from lists that are not as an earlier herdr-run wrote them; add them by hand to adopt them:"
                .to_owned(),
        );
        for item in &result.missing {
            let entries: Vec<&str> = item.entries.iter().map(String::as_str).collect();
            lines.push(format!("  {}: {}", item.setting, quoted(&entries)));
        }
    }
    output::stdout(
        &lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>(),
    )?;
    Ok(0)
}

/// Report what is in effect here, and change nothing while doing it.
fn command_status(config: &Config, agent: &str, json_output: bool) -> Result<i32> {
    use crate::status::{inspect_session, status_document, status_text, Session};

    let tab_label = tab_label_for(config, agent)?;
    // A resolution failure is a fact about the session worth REPORTING, not an error to exit on:
    // "herdr is not installed" is one of the most useful things status can tell somebody.
    let client = HerdrClient::new(&config.broker)?;
    let session = match client.preflight() {
        Ok(()) => inspect_session(&client, config, &tab_label),
        Err(error) => Session::Unreachable(error.message().to_owned()),
    };
    if json_output {
        print_json(&status_document(config, agent, &tab_label, &session))?;
    } else {
        output::stdout(&status_text(config, agent, &tab_label, &session))?;
    }
    Ok(0)
}

fn command_config(config: &Config, agent: &str) -> Result<i32> {
    let document = json!({
        "allow": config.allow,
        "allow_subcommand": config.allow_subcommand,
        "broker": config.broker,
        "cwd": config.cwd,
        "deny_anywhere": config.deny_anywhere,
        "deny_global": config.deny_global,
        "deny_subcommand": config.deny_subcommand,
        "deny_subcommand_option": config.deny_subcommand_option,
        "max_panes": config.max_panes,
        "prefixes": config.prefixes,
        "probe_remote": config.probe_remote,
        "project_root": config.project_root,
        "prompt_tail": config.prompt_tail,
        "readiness": config.readiness,
        "ready_timeout_seconds": config.ready_timeout_seconds,
        "retention_days": config.retention_days,
        "shells": config.shells,
        "source": config.source_path.as_deref().unwrap_or("(built-in defaults)"),
        "spool_dir": config.spool_dir,
        "tab_label": tab_label_for(config, agent)?,
        "tab_name": config.tab_name,
        "timeout_seconds": config.timeout_seconds,
        "value_options": config.value_options,
        "workspace": config.workspace,
    });
    print_json(&document)?;
    Ok(0)
}

/// The two lines `check` prints for an admitted command.
///
/// The program, the subcommand, and the rendered command are each shown with [`display`]: as they
/// are, unless one holds a character a name may not contain -- an invisible or bidirectional
/// character, a control, a noncharacter, a private-use character, or an unassigned code point --
/// in which case that value is quoted with the character escaped, so it never reaches the
/// terminal raw. The subcommand and every argument are whatever was typed, and under
/// `allow: ["*"]` so is the program. `-` stands for an ABSENT subcommand only (a subcommand never
/// starts with `-`); an empty one is printed as empty, the way the JSON form distinguishes `null`
/// from `""`.
#[must_use]
pub fn check_text(program: &str, subcommand: Option<&str>, rendered: &str) -> String {
    format!(
        "ALLOWED: program={} subcommand={}\nrendered: {}\n",
        display(program),
        subcommand.map_or_else(|| "-".to_owned(), display),
        display(rendered)
    )
}

fn command_check(config: &Config, command: &str, json_output: bool) -> Result<i32> {
    match admit(command, config) {
        Ok(admission) => {
            if json_output {
                print_json(&json!({
                    "program": admission.program,
                    "rendered": admission.rendered(),
                    "subcommand": admission.subcommand,
                    "verdict": "allowed",
                }))?;
            } else {
                output::stdout(&check_text(
                    &admission.program,
                    admission.subcommand.as_deref(),
                    admission.rendered(),
                ))?;
            }
            Ok(0)
        }
        Err(error) if error.kind() == ErrorKind::Refused => {
            if json_output {
                print_json(&json!({
                    "reason": error.message(),
                    "verdict": "refused",
                }))?;
            } else {
                output::diagnostic(&format!("REFUSED: {error}"));
            }
            Ok(error.exit_code())
        }
        Err(error) => Err(error),
    }
}

fn command_target(config: &Config, agent: &str, use_cache: bool) -> Result<i32> {
    let client = HerdrClient::new(&config.broker)?;
    let target = resolve_target(&client, config, agent, use_cache)?;
    let prompt_tail = infer_prompt_tail(config, None);
    let readiness = assess(&client, &target.pane_id, config, prompt_tail.as_deref(), 4)?;
    let document = json!({
        "created": target.created,
        "from_cache": target.from_cache,
        "pane_id": target.pane_id,
        "prompt_tail": prompt_tail,
        "readiness": readiness.describe(),
        "ready": readiness.ready,
        "tab": {"id": target.tab_id, "label": target.tab_label},
        "workspace": {"id": target.workspace_id, "label": target.workspace_label},
    });
    print_json(&document)?;
    Ok(if readiness.ready { 0 } else { EXIT_BUSY })
}

fn run_command(config: &Config, agent: &str, json_output: bool, args: &RunOptions) -> Result<i32> {
    let command = args.command.as_str();
    let log = audit::audit_path(
        Path::new(&config.project_root),
        Path::new(&config.spool_dir),
    );
    let _ = audit::warn_if_spool_is_tracked(
        Path::new(&config.project_root),
        Path::new(&config.spool_dir),
    );
    let admission = match admit(command, config) {
        Ok(admission) => admission,
        Err(error) if error.kind() == ErrorKind::Refused => {
            record_audit(&log, agent, command, "REFUSED", error.message(), Map::new());
            output::diagnostic(&format!("herdr-run: REFUSED: {error}"));
            return Ok(error.exit_code());
        }
        Err(error) => return Err(error),
    };
    if args.dry_run {
        record_audit(
            &log,
            agent,
            command,
            "DRY-RUN",
            admission.rendered(),
            Map::new(),
        );
        if json_output {
            print_json(&json!({
                "program": admission.program,
                "rendered": admission.rendered(),
                "verdict": "allowed",
            }))?;
        } else {
            output::stdout(&format!("{}\n", admission.rendered()))?;
        }
        return Ok(0);
    }

    let mut admitted_fields = Map::new();
    admitted_fields.insert("rendered".to_owned(), json!(admission.rendered()));
    record_audit(
        &log,
        agent,
        command,
        "ADMITTED",
        "policy accepted command; target resolution and launch have not yet completed",
        admitted_fields,
    );

    let client = match HerdrClient::new(&config.broker) {
        Ok(client) => client,
        Err(error) => return audited_error(&log, agent, command, error),
    };
    let cwd = match resolved_cwd(config, args.cwd.as_deref()) {
        Ok(cwd) => cwd,
        Err(error) => return audited_error(&log, agent, command, error),
    };
    let ready_timeout = args.wait_ready.unwrap_or(config.ready_timeout_seconds);
    let timeout = args.timeout.unwrap_or(config.timeout_seconds);
    let target = match resolve_target(&client, config, agent, !args.no_cache) {
        Ok(target) => target,
        Err(error) => return audited_error(&log, agent, command, error),
    };
    let result = match execute(
        &client,
        config,
        &target,
        &admission,
        agent,
        &cwd,
        ready_timeout,
        timeout,
    ) {
        Ok(result) => result,
        Err(error) if error.kind() == ErrorKind::Timeout => {
            // Emit whatever the command managed to print BEFORE reporting the timeout: dropping it
            // would make a partially successful run look like one that produced nothing.
            record_audit(
                &log,
                agent,
                command,
                "RUNTIMEOUT",
                error.message(),
                Map::new(),
            );
            output::write_labelled(1, error.partial_stdout().as_bytes(), "partial stdout")?;
            output::write_labelled(2, error.partial_stderr().as_bytes(), "partial stderr")?;
            output::diagnostic(&format!("herdr-run: {error}"));
            return Ok(error.exit_code());
        }
        Err(error) => return audited_error(&log, agent, command, error),
    };
    let (meta, meta_error) = write_meta_best_effort(&result, &admission, config, agent);
    let mut fields = Map::new();
    fields.insert("exit_code".to_owned(), json!(result.exit_code));
    fields.insert("meta".to_owned(), json!(meta.as_ref()));
    if let Some(error) = meta_error {
        fields.insert("meta_error".to_owned(), json!(error));
    }
    fields.insert("pane_id".to_owned(), json!(result.target.pane_id));
    fields.insert("rendered".to_owned(), json!(admission.rendered()));
    fields.insert("run_id".to_owned(), json!(result.run_id));
    fields.insert("tab".to_owned(), json!(result.target.tab_label));
    record_audit(
        &log,
        agent,
        command,
        "RAN",
        &format!(
            "exit {} in {:.1}s",
            result.exit_code, result.duration_seconds
        ),
        fields,
    );
    if json_output {
        print_json(&result_json(&result, meta.as_deref()))?;
    } else {
        output::stdout_bytes(&result.stdout)?;
        output::stderr_bytes(&result.stderr)?;
    }
    Ok(result.exit_code)
}

fn audited_error(log: &Path, agent: &str, command: &str, error: HerdrRunError) -> Result<i32> {
    record_audit(
        log,
        agent,
        command,
        error_verdict(error.kind()),
        error.message(),
        Map::new(),
    );
    output::diagnostic(&format!("herdr-run: {error}"));
    Ok(error.exit_code())
}

fn record_audit(
    log: &Path,
    agent: &str,
    command: &str,
    verdict: &str,
    detail: &str,
    fields: Map<String, Value>,
) {
    if !audit::record(log, agent, command, verdict, detail, fields) {
        output::diagnostic(&format!(
            "herdr-run: WARNING: could not append audit record to {}",
            log.display()
        ));
    }
}

fn write_meta_best_effort(
    result: &RunResult,
    admission: &Admission,
    config: &Config,
    agent: &str,
) -> (Option<PathBuf>, Option<String>) {
    match write_meta(result, admission, config, agent) {
        Ok(path) => (Some(path), None),
        Err(error) => {
            output::diagnostic(&format!("herdr-run: WARNING: {}", error.message()));
            (None, Some(error.message().to_owned()))
        }
    }
}

/// Report which command tabs are PROVABLY finished with. Closes nothing.
///
/// Report-only is the point, not a limitation. The expensive mistake is closing a tab whose agent
/// is merely thinking, and a reaper that is wrong once in that direction is switched off for good;
/// so the first version of this has to be checkable against a known-good population before anyone
/// lets it act. Every declined pane carries its reason, and every verdict is counted including the
/// zeros, because "reaped 0 because nothing was stale" and "reaped 0 because the detector is inert"
/// are otherwise the same output.
///
/// `candidate_source` states the bound on what could POSSIBLY have been considered. The candidate
/// set is the panes named by surviving run records, and herdr-run prunes a run record
/// `retention_days` after the run finished — so the oldest leaked tabs, which are exactly the ones
/// the pane cap exists to bound, drop out of this report while still counting against `max_panes`.
/// Printing the window is the difference between a report an operator can reason about and a count
/// that quietly means something narrower than it says.
///
/// `occupancy` measures that gap instead of only naming it: herdr's live pane list, which is what
/// the cap counts, split by whether a surviving run record names each pane. Measured 2026-10-02 on
/// devbig030, the cap was held by live panes with no record while `reap` listed only records, so
/// its output could not say what was holding the cap.
///
/// `replaceable` says whether that matters: the cap replaces an idle unsplit tab before it refuses,
/// so a full workspace with an idle tab still admits the next agent. It is judged as the cap
/// judges except for the pane lock, by [`evict::survey`], which closes nothing.
///
/// The three readings are taken in one fixed order in both editions: the evidence for every
/// recorded pane first, then the occupancy listing, then the survey.
fn command_reap(config: &Config) -> Result<i32> {
    let client = HerdrClient::new(&config.broker)?;
    let records = load_run_records(config);
    let plan = plan_reap(&build_evidence(
        &client,
        config,
        Some(records.clone()),
        Path::new("/proc"),
    ));
    let occupancy = measure_occupancy(&client, config, &records);
    let survey = occupancy
        .listing_error
        .is_none()
        .then(|| evict::survey_workspace(&client, config, &records, Path::new("/proc")));
    let (replaceable, cap_refusal, survey_error) = match &survey {
        Some(Ok(survey)) => (
            Some(survey.replaceable.len()),
            (survey.replaceable.is_empty() && !survey.kept.is_empty())
                .then(|| evict::describe_skipped(&survey.kept)),
            None,
        ),
        Some(Err(error)) => (None, None, Some(error.to_string())),
        None => (None, None, None),
    };
    let entry = |decision: &crate::reap::ReapDecision, with_verdict: bool| {
        let mut object = Map::new();
        object.insert("pane_id".to_owned(), json!(decision.pane_id));
        object.insert("reason".to_owned(), json!(decision.reason));
        object.insert("tab_id".to_owned(), json!(decision.tab_id));
        object.insert("tab_label".to_owned(), json!(decision.tab_label));
        if with_verdict {
            object.insert("verdict".to_owned(), json!(decision.verdict.as_str()));
        }
        Value::Object(object)
    };
    let document = json!({
        "occupancy": {
            "listing_error": occupancy.listing_error,
            "live_panes": occupancy.live_panes,
            "max_panes": config.max_panes,
            "cap_refusal": cap_refusal,
            "note": OCCUPANCY_NOTE,
            "replaceable": replaceable,
            "survey_error": survey_error,
            "with_record": occupancy.with_record,
            "without_record": occupancy.without_record(),
        },
        "candidate_source": {
            "note": CANDIDATE_SOURCE_NOTE,
            "retention_days": config.retention_days,
            "spool_dir": config.spool_dir,
        },
        "counts": plan.counts(),
        "declined": plan.declined().into_iter().map(|d| entry(d, true)).collect::<Vec<_>>(),
        "reapable": plan.reapable().into_iter().map(|d| entry(d, false)).collect::<Vec<_>>(),
        "workspace": config.workspace,
    });
    print_json(&document)?;
    Ok(0)
}

/// The bound on `reap`'s candidate set, printed with every report.
///
/// A constant rather than an inline literal because the wording is part of the command's stable
/// output and is compared verbatim by the differential harness.
const CANDIDATE_SOURCE_NOTE: &str = concat!(
    "candidates are the panes named by surviving run records; run records are pruned ",
    "retention_days after the run finished, so a tab whose agent last ran longer ago than that ",
    "is not considered here; at the cap it is replaced first if its shell is idle, otherwise it ",
    "must be closed by hand"
);

/// How to read `reap`'s `occupancy` block. Compared verbatim by the differential harness.
const OCCUPANCY_NOTE: &str = concat!(
    "max_panes counts every pane herdr lists in the workspace (live_panes); reap can judge only ",
    "the panes a surviving run record names (with_record), so without_record panes hold their ",
    "share of max_panes until closed by hand or replaced as idle at the cap; replaceable counts ",
    "the tabs the cap would replace if an agent arrived now (unsplit, shell idle), judged without ",
    "the pane lock and closing nothing, and cap_refusal is what that agent would be told when ",
    "none is"
);

/// The scope disclaimer `net-doctor` prints before it does anything.
///
/// It goes FIRST, not in the verdict, because a diagnostic that only says what it covered once it
/// has already failed reads as an excuse. `net-doctor` answers exactly one question — does routing
/// a network command through a pane get past a block on the caller's own network — and a reader
/// whose interest in this command is something else should be able to stop at line one.
const NET_DOCTOR_SCOPE: &str = concat!(
    "This is a smoke test for ONE scenario, not a health check of this command as a whole:\n",
    "a caller whose own network access is blocked, running a network command through a pane\n",
    "that is not blocked. It runs the same probe twice — directly, then through the pane —\n",
    "and compares. If you route commands through a pane for some other reason, the verdict\n",
    "below says nothing about that reason.\n",
);

/// The first line of `text` after trimming, splitting on line feeds only.
fn first_line(text: &str) -> &str {
    strip_space(text).split('\n').next().unwrap_or("")
}

/// The last line of `text` after trimming, splitting on line feeds only.
fn last_line(text: &str) -> &str {
    strip_space(text).split('\n').next_back().unwrap_or("")
}

fn command_net_doctor(config: &Config, agent: &str) -> Result<i32> {
    let mut argv = vec![
        "git".to_owned(),
        "ls-remote".to_owned(),
        config.probe_remote.clone(),
        "HEAD".to_owned(),
    ];
    if config.prefixes.iter().any(|prefix| prefix == "with-proxy") {
        argv.insert(0, "with-proxy".to_owned());
    }
    output::stdout(&format!(
        "herdr-run net-doctor  (agent={}, config={})\n\n{NET_DOCTOR_SCOPE}\n",
        display(agent),
        config
            .source_path
            .as_deref()
            .map_or_else(|| "defaults".to_owned(), display)
    ))?;
    let log = audit::audit_path(
        Path::new(&config.project_root),
        Path::new(&config.spool_dir),
    );
    let _ = audit::warn_if_spool_is_tracked(
        Path::new(&config.project_root),
        Path::new(&config.spool_dir),
    );
    // Admitted exactly as `run` admits a command: its words quoted as written, `~` expanded by the
    // admission and judged expanded, so the probe that is printed, audited, and executed is the
    // rendered line the policy admitted.
    let command = argv
        .iter()
        .map(|token| quote_word(token))
        .collect::<Vec<_>>()
        .join(" ");
    let admission = match admit(&command, config) {
        Ok(admission) => admission,
        Err(error) => {
            record_audit(
                &log,
                agent,
                &command,
                error_verdict(error.kind()),
                error.message(),
                net_doctor_error_fields(None),
            );
            return Err(error);
        }
    };
    let probe = admission.rendered().to_owned();
    let mut admitted_fields = Map::new();
    admitted_fields.insert("net_doctor".to_owned(), json!(true));
    admitted_fields.insert("rendered".to_owned(), json!(admission.rendered()));
    record_audit(
        &log,
        agent,
        &probe,
        "ADMITTED",
        "net-doctor probe accepted; target resolution and launch have not yet completed",
        admitted_fields,
    );
    let mut direct_command = Command::new("/bin/bash");
    direct_command.args(["-lc", &probe]);
    let direct = match bounded_output(&mut direct_command, std::time::Duration::from_secs(120)) {
        Ok(output) => output,
        Err(error) => {
            let error = HerdrRunError::unavailable(format!(
                "cannot run the direct probe: {}",
                os_error_text(&error)
            ));
            record_audit(
                &log,
                agent,
                &probe,
                error_verdict(error.kind()),
                error.message(),
                net_doctor_error_fields(None),
            );
            return Err(error);
        }
    };
    let direct_code = exit_status_code(direct.status);
    let direct_ok = direct_code == 0;
    let mut direct_text = format!(
        "[direct  ] {probe}\n           rc={direct_code} {}\n",
        if direct_ok { "SUCCEEDED" } else { "failed" }
    );
    if !direct_ok {
        let stderr = String::from_utf8_lossy(&direct.stderr);
        direct_text.push_str(&format!("           {}\n", last_line(&stderr)));
    }
    output::stdout(&direct_text)?;

    // Every failure after the direct half of the diagnosis is a net-doctor result, not a raw
    // wrapper exit. Keep construction, target/cwd resolution, and execution in one result so they
    // all produce the same `[via pane] FAILED` line, final verdict, exit 1, and audit metadata.
    let outside: Result<_> = (|| {
        let client = HerdrClient::new(&config.broker)?;
        let target = resolve_target(&client, config, agent, true)?;
        let cwd = resolved_cwd(config, None)?;
        let result = execute(
            &client,
            config,
            &target,
            &admission,
            agent,
            &cwd,
            config.ready_timeout_seconds.max(30.0),
            config.timeout_seconds.min(180.0),
        )?;
        Ok((target, result))
    })();
    let pane_code = match outside {
        Ok((target, result)) => {
            let (meta, meta_error) = write_meta_best_effort(&result, &admission, config, agent);
            let mut fields = net_doctor_error_fields(Some(direct_code));
            fields.insert("exit_code".to_owned(), json!(result.exit_code));
            fields.insert("run_id".to_owned(), json!(result.run_id));
            fields.insert("pane_id".to_owned(), json!(result.target.pane_id));
            fields.insert("meta".to_owned(), json!(meta.as_ref()));
            if let Some(error) = meta_error {
                fields.insert("meta_error".to_owned(), json!(error));
            }
            record_audit(
                &log,
                agent,
                &probe,
                "RAN",
                &format!("net-doctor pane probe exited {}", result.exit_code),
                fields,
            );
            let stdout = result.stdout_text();
            let head: String = first_line(&stdout).chars().take(80).collect();
            output::stdout(&format!(
                "[via pane] {probe}   (pane {}, tab {})\n           rc={} {}  {head}\n",
                target.pane_id,
                target.tab_label,
                result.exit_code,
                if result.exit_code == 0 {
                    "SUCCEEDED"
                } else {
                    "failed"
                }
            ))?;
            result.exit_code
        }
        Err(error) => {
            let fields = net_doctor_error_fields(Some(direct_code));
            record_audit(
                &log,
                agent,
                &probe,
                error_verdict(error.kind()),
                error.message(),
                fields,
            );
            output::stdout(&format!("[via pane] FAILED: {error}\n"))?;
            -1
        }
    };
    if pane_code == 0 && !direct_ok {
        output::stdout("\nVERDICT: this scenario works — the probe is blocked directly and succeeds through the pane.\n")?;
        Ok(0)
    } else if pane_code == 0 {
        output::stdout("\nVERDICT: the pane works, but so does running the probe directly. For THIS scenario the pane is buying you nothing; prefer running the command directly. Other reasons to route a command through a pane are untouched by that.\n")?;
        Ok(0)
    } else {
        output::stdout("\nVERDICT: the pane path is NOT working. Most likely the Herdr server was started from inside a confined process, so its panes inherit the confinement. Stop it ('herdr server stop') and let herdr-run restart it via systemd-run, or start it from an unconfined shell.\n")?;
        Ok(1)
    }
}

fn error_verdict(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Config => "CONFIGERROR",
        ErrorKind::Refused => "REFUSED",
        ErrorKind::Unavailable => "HERDRUNAVAILABLE",
        ErrorKind::Busy => "PANEBUSY",
        ErrorKind::Timeout => "RUNTIMEOUT",
        ErrorKind::Other => "HERDRRUNERROR",
    }
}

fn net_doctor_error_fields(direct_exit_code: Option<i32>) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("net_doctor".to_owned(), json!(true));
    if let Some(code) = direct_exit_code {
        fields.insert("direct_exit_code".to_owned(), json!(code));
    }
    fields
}

fn resolved_cwd(config: &Config, override_cwd: Option<&str>) -> Result<PathBuf> {
    resolved_cwd_with(config, override_cwd, std::env::current_dir)
}

/// Where the command runs: `--cwd`, else the configured `cwd`, else the caller's directory.
///
/// An absolute path is passed through exactly as written and the pane's own `cd` resolves it:
/// resolving `..` lexically here could pick a different directory than the shell would when a
/// symbolic link precedes it. A relative path is resolved against the project root.
fn resolved_cwd_with<F>(
    config: &Config,
    override_cwd: Option<&str>,
    current_dir: F,
) -> Result<PathBuf>
where
    F: FnOnce() -> io::Result<PathBuf>,
{
    let Some(raw) = override_cwd.or(config.cwd.as_deref()) else {
        return current_dir().map_err(|error| {
            HerdrRunError::config(format!(
                "cannot determine current directory: {}",
                os_error_text(&error)
            ))
        });
    };
    let path = Path::new(raw);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(lexical_normalize(
        &Path::new(&config.project_root).join(path),
    ))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => output.push(prefix.as_os_str()),
            Component::RootDir => output.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            Component::Normal(part) => output.push(part),
        }
    }
    output
}

fn print_json(value: &Value) -> Result<()> {
    output::stdout(&format!("{}\n", json_pretty(value)))
}

fn usage_line() -> &'static str {
    "herdr-run [-h] [--version] [--config PATH] [--agent NAME] [--json] <subcommand> [OPTIONS]"
}

/// Build the top-level help so it can be asserted on as text, not only printed.
fn help_text() -> String {
    let mut text = String::new();
    text.push_str(&format!(
        "usage: {}
",
        usage_line()
    ));
    text.push_str("\nRun an allowlisted command in a terminal pane that is not subject to whatever\nconstrains the caller, and get its real stdout, stderr, and exit code back.\n");
    text.push_str("\nsubcommands:\n");
    for subcommand in SUBCOMMANDS {
        text.push_str(&format!(
            "  {:<11} {}\n",
            subcommand.name, subcommand.summary
        ));
    }
    text.push_str("\nglobal options (before the subcommand):\n  -h, --help            show this help message and exit\n  --version             show version and exit\n  --config PATH         explicit configuration file\n  --agent NAME          the agent this invocation speaks for; names its tab\n  --json                emit machine-readable output where a subcommand has it\n  --dump-surface        print every subcommand and option as JSON and exit\n");
    text.push_str("\nEach subcommand has its own options: run 'herdr-run <subcommand> --help'.\nOptions are not shared between the two levels — '--cwd' is a 'run' option and\ngoes after 'run'; '--agent' is global and goes before it.\n");
    text.push_str("\nTwo documentation commands, and they are for different moments:\n  quickstart  one screen. What this is for, the four commands worth trying\n              first, and the five things to know before running anything real.\n  userguide   the complete reference: configuration, exit codes, readiness,\n              retention, the pane cap, and the trust model.\n");
    text
}

fn print_help() -> Result<()> {
    output::stdout(&help_text())
}

/// Said by every subcommand that `config_blind_reason` names, so the refusal is never a surprise.
const CONFIG_BLIND_NOTE: &str = "\nThis command takes no configuration file, so the global --config option is\nrefused here rather than accepted and ignored.\n";

/// Said by every subcommand that `agent_blind_reason` names.
const AGENT_BLIND_NOTE: &str = "\nThis command names no tab, so the global --agent option is refused here rather\nthan accepted and ignored.\n";

/// Said by every subcommand that `json_blind_reason` names.
const JSON_BLIND_NOTE: &str = "\nThis command has no machine-readable output, so the global --json option is\nrefused here rather than accepted and ignored.\n";

/// Build one subcommand's help so it can be asserted on as text, not only printed.
fn subcommand_help_text(name: &str) -> String {
    let mut text = String::new();
    match name {
        "run" => {
            text.push_str("usage: herdr-run [GLOBAL OPTIONS] run [OPTIONS] '<command>'\n");
            text.push_str("\nRun one allowlisted command in this agent's pane and return its stdout, stderr,\nand exit code. The command is ONE argument: quote it.\n");
            text.push_str("\npositional arguments:\n  <command>             the whole command line, as a single quoted argument\n");
            text.push_str("\noptions:\n  -h, --help            show this help message and exit\n  --cwd PATH            working directory for the command\n  --timeout SECONDS     how long to wait for the command to finish\n  --wait-ready SECONDS  how long to wait for the pane to go idle\n  --no-cache            ignore the session cache and re-resolve from labels\n  --dry-run             admit and render the command; execute nothing\n");
            text.push_str(
                "\nExample:\n  herdr-run --agent release-agent run 'git push origin HEAD'\n",
            );
        }
        "check" => {
            text.push_str("usage: herdr-run [GLOBAL OPTIONS] check '<command>'\n");
            text.push_str("\nDecide whether a command would be admitted by the policy in effect. Touches no\npane and executes nothing. Exits 0 when allowed and 77 when refused.\n");
            text.push_str("\nWhen allowed, prints 'ALLOWED: program=P subcommand=S' (S is '-' when there is\nno subcommand) and 'rendered: C', the command line as run would type it. A value\nholding an invisible or bidirectional character, or any other character a name\nmay not contain, is shown quoted with that character escaped as \\u{hex}.\n");
            text.push_str("\npositional arguments:\n  <command>             the whole command line, as a single quoted argument\n");
            text.push_str("\noptions:\n  -h, --help            show this help message and exit\n");
            text.push_str("\nExample:\n  herdr-run check 'git push origin HEAD'\n");
        }
        "init" => {
            text.push_str("usage: herdr-run [GLOBAL OPTIONS] init [OPTIONS]\n");
            text.push_str("\nWrite an annotated .herdr-run.yaml into the current directory, the way 'git init'\nwrites into the current directory. Every knob is present and set to the value in\nforce today, so adopting the file changes nothing until you edit it. Refuses to\noverwrite an existing configuration.\n");
            text.push_str("\nWith --upgrade, edit the .herdr-run.yaml (or .herdr-run.yml) already in the current\ndirectory instead. Each setting it still holds exactly as an earlier herdr-run wrote\nit is rewritten as this one writes it, which adopts the built-in deny entries added\nsince; nothing else in the file changes. Built-in deny entries it still leaves out,\nin lists somebody edited, are listed to be added by hand.\n");
            text.push_str("\noptions:\n  -h, --help            show this help message and exit\n  --force               overwrite an existing configuration file\n  --upgrade             bring the settings an earlier herdr-run wrote up to date\n");
        }
        "status" => {
            text.push_str("usage: herdr-run [GLOBAL OPTIONS] status\n");
            text.push_str("\nReport what is in effect here: which configuration file was found and where its\nroot is, what the policy admits, whether Herdr is reachable and running, and how\nmany panes the workspace already holds. Strictly non-mutating — it starts no\nserver and creates no workspace, tab, or pane.\n");
            text.push_str("\noptions:\n  -h, --help            show this help message and exit\n");
        }
        "config" => {
            text.push_str("usage: herdr-run [GLOBAL OPTIONS] config\n");
            text.push_str("\nPrint the fully resolved configuration as JSON: every value in effect, the file\nit came from, and the tab label it renders for this agent.\n");
            text.push_str("\noptions:\n  -h, --help            show this help message and exit\n");
        }
        "target" => {
            text.push_str("usage: herdr-run [GLOBAL OPTIONS] target [OPTIONS]\n");
            text.push_str("\nResolve this agent's pane, creating the workspace, tab, and pane if they are\nmissing, and print their ids together with the readiness verdict.\n");
            text.push_str("\noptions:\n  -h, --help            show this help message and exit\n  --no-cache            ignore the session cache and re-resolve from labels\n");
        }
        "reap" => {
            text.push_str("usage: herdr-run [GLOBAL OPTIONS] reap\n");
            text.push_str("\nReport which command tabs are provably finished with, and why every other one\nwas declined. Closes nothing.\n");
            text.push_str("\noptions:\n  -h, --help            show this help message and exit\n");
        }
        "net-doctor" => {
            text.push_str("usage: herdr-run [GLOBAL OPTIONS] net-doctor\n");
            text.push_str("\nSmoke-test one narrow scenario: a caller whose own network access is blocked,\nreaching the network through a pane that is not blocked. It runs one probe\ndirectly and again through the pane and compares the two. This is not a health\ncheck of the command as a whole.\n");
            text.push_str("\noptions:\n  -h, --help            show this help message and exit\n");
        }
        "quickstart" => {
            text.push_str("usage: herdr-run [GLOBAL OPTIONS] quickstart\n");
            text.push_str("\nPrint the one-screen introduction: what this command is for, the four commands\nworth trying first, the shape of the command line, and the five things worth\nknowing before running anything real.\n");
            text.push_str("\noptions:\n  -h, --help            show this help message and exit\n");
        }
        "userguide" => {
            text.push_str("usage: herdr-run [GLOBAL OPTIONS] userguide\n");
            text.push_str("\nPrint the complete reference: configuration, exit codes, readiness, retention,\nthe pane cap, and the trust model.\n");
            text.push_str("\noptions:\n  -h, --help            show this help message and exit\n");
        }
        other => text.push_str(&format!("usage: herdr-run [GLOBAL OPTIONS] {other}\n")),
    }
    // Derived from the very tables the refusals are decided by, rather than written out again per
    // subcommand, so a subcommand cannot come to refuse a global option without saying so here.
    // Listed in the order the global option block lists them.
    if config_blind_reason(name).is_some() {
        text.push_str(CONFIG_BLIND_NOTE);
    }
    if agent_blind_reason(name).is_some() {
        text.push_str(AGENT_BLIND_NOTE);
    }
    if json_blind_reason(name).is_some() {
        text.push_str(JSON_BLIND_NOTE);
    }
    text
}

fn print_subcommand_help(name: &str) -> Result<()> {
    output::stdout(&subcommand_help_text(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(arguments: &[&str]) -> std::result::Result<Invocation, String> {
        match parse(arguments.iter().map(OsString::from)) {
            Ok(ParseResult::Invocation(invocation)) => Ok(*invocation),
            Ok(ParseResult::Exit(code)) => Err(format!("unexpected early exit {code}")),
            Err(ParseFailure::Usage(message)) => Err(message),
            Err(ParseFailure::Failed(error)) => Err(error.to_string()),
        }
    }

    #[test]
    fn global_options_are_accepted_before_the_subcommand() {
        let invocation = parse_args(&[
            "--config",
            "/tmp/c.yaml",
            "--agent",
            "release-agent",
            "--json",
            "run",
            "git status",
        ])
        .expect("parse");
        assert_eq!(
            invocation.globals,
            Globals {
                config: Some(PathBuf::from("/tmp/c.yaml")),
                agent: Some("release-agent".to_owned()),
                json: true,
            }
        );
        assert_eq!(
            invocation.selected,
            Selected::Run(RunOptions {
                command: "git status".to_owned(),
                ..RunOptions::default()
            })
        );
    }

    #[test]
    fn run_options_are_accepted_after_the_subcommand() {
        let invocation = parse_args(&[
            "run",
            "--cwd",
            "/tmp",
            "--timeout=12.5",
            "--wait-ready",
            "3",
            "--no-cache",
            "--dry-run",
            "git status",
        ])
        .expect("parse");
        assert_eq!(
            invocation.selected,
            Selected::Run(RunOptions {
                command: "git status".to_owned(),
                cwd: Some("/tmp".to_owned()),
                timeout: Some(12.5),
                wait_ready: Some(3.0),
                no_cache: true,
                dry_run: true,
            })
        );
    }

    /// The two levels must not leak into each other, in either direction.
    ///
    /// This is the defect the surface exists to fix, so it is pinned as behaviour rather than left
    /// to the help text: an accepted `herdr-run --cwd /tmp run ...` would quietly restore the very
    /// mixing that made the old surface unreadable.
    #[test]
    fn an_option_at_the_wrong_level_is_refused_and_says_which_level_it_belongs_to() {
        for option in [
            "--cwd",
            "--timeout",
            "--wait-ready",
            "--no-cache",
            "--dry-run",
        ] {
            let error = parse_args(&[option, "1", "run", "git status"]).expect_err(option);
            assert!(
                error.contains("put it AFTER the subcommand"),
                "{option}: {error}"
            );
        }
        for option in ["--config", "--agent"] {
            let error = parse_args(&["run", option, "value", "git status"]).expect_err(option);
            assert!(
                error.contains("this is a GLOBAL option"),
                "{option}: {error}"
            );
        }
        let error = parse_args(&["--json", "check", "--dry-run", "git status"])
            .expect_err("check --dry-run");
        assert!(
            error.contains("this is a 'run' option, not a 'check' option"),
            "{error}"
        );
        let error = parse_args(&["target", "--dry-run"]).expect_err("target --dry-run");
        assert!(
            error.contains("this is a 'run' option, not a 'target' option"),
            "{error}"
        );
        // --no-cache belongs to two subcommands, and both must be named.
        let error = parse_args(&["--no-cache", "target"]).expect_err("global --no-cache");
        assert!(error.contains("'run' or 'target'"), "{error}");
    }

    /// A global option a subcommand cannot observe must be refused, not accepted and ignored.
    ///
    /// `--config` is not inert: it reads as an instruction about which configuration file to use,
    /// and `herdr-run --config P init` reads as an instruction to write `P`. It used to be
    /// accepted and silently disregarded, which is the same accept-anything defect the two-level
    /// surface exists to remove.
    #[test]
    fn config_is_refused_by_the_subcommands_that_read_no_configuration() {
        for arguments in [
            vec!["--config", "x.yaml", "init"],
            vec!["--config=x.yaml", "init"],
            vec!["--config", "x.yaml", "init", "--force"],
            vec!["--config", "x.yaml", "init", "--upgrade"],
            vec!["--config", "x.yaml", "quickstart"],
            vec!["--config", "x.yaml", "userguide"],
        ] {
            let error = parse_args(&arguments).expect_err(&arguments.join(" "));
            assert!(
                error.contains("argument --config:")
                    && error.contains("takes no configuration file"),
                "{arguments:?}: {error}"
            );
        }
        let error = parse_args(&["--config", "x.yaml", "init"]).expect_err("init");
        assert!(error.contains("cd DIRECTORY && herdr-run init"), "{error}");

        // The subcommands that DO read configuration are untouched.
        for subcommand in ["status", "config", "reap", "net-doctor", "target"] {
            parse_args(&["--config", "x.yaml", subcommand])
                .unwrap_or_else(|error| panic!("{subcommand}: {error}"));
        }
    }

    /// `--agent NAME` is an instruction too: it says who this invocation speaks for.
    ///
    /// The only thing that does is name a tab, so a subcommand that resolves no tab has nothing to
    /// do with it. It used to be accepted and dropped, which reads as agreement.
    #[test]
    fn agent_is_refused_by_the_subcommands_that_name_no_tab() {
        for arguments in [
            vec!["--agent", "a", "check", "git status"],
            vec!["--agent=a", "check", "git status"],
            vec!["--agent", "a", "init"],
            vec!["--agent", "a", "reap"],
            vec!["--agent", "a", "quickstart"],
            vec!["--agent", "a", "userguide"],
        ] {
            let error = parse_args(&arguments).expect_err(&arguments.join(" "));
            assert!(
                error.contains("argument --agent:") && error.contains("names no tab"),
                "{arguments:?}: {error}"
            );
        }

        // The subcommands that DO name a tab are untouched.
        for arguments in [
            vec!["--agent", "a", "run", "git status"],
            vec!["--agent", "a", "status"],
            vec!["--agent", "a", "config"],
            vec!["--agent", "a", "target"],
            vec!["--agent", "a", "net-doctor"],
        ] {
            parse_args(&arguments)
                .unwrap_or_else(|error| panic!("{}: {error}", arguments.join(" ")));
        }
    }

    /// `--json` asks for machine-readable output, and prose is not a machine-readable answer.
    ///
    /// A caller who is refused finds out at the flag; a caller who is humoured finds out at the
    /// parse of the output, where the failure looks like corrupt data instead of a bad request.
    #[test]
    fn json_is_refused_by_the_subcommands_with_no_machine_readable_output() {
        for arguments in [
            vec!["--json", "net-doctor"],
            vec!["--json", "quickstart"],
            vec!["--json", "userguide"],
        ] {
            let error = parse_args(&arguments).expect_err(&arguments.join(" "));
            assert!(
                error.contains("argument --json:")
                    && error.contains("has no machine-readable output"),
                "{arguments:?}: {error}"
            );
        }

        // Four subcommands change what they print for `--json`, and three print JSON either way.
        // Neither group may refuse it.
        for arguments in [
            vec!["--json", "run", "git status"],
            vec!["--json", "check", "git status"],
            vec!["--json", "status"],
            vec!["--json", "init"],
            vec!["--json", "config"],
            vec!["--json", "target"],
            vec!["--json", "reap"],
        ] {
            parse_args(&arguments)
                .unwrap_or_else(|error| panic!("{}: {error}", arguments.join(" ")));
        }
    }

    /// Every refusal must be announced in the help of the subcommand that makes it, and nowhere
    /// else. A refusal a caller can only discover by tripping over it is a trap, and a note on a
    /// subcommand that accepts the option is a lie.
    #[test]
    fn every_blind_subcommand_says_so_in_its_own_help() {
        for subcommand in SUBCOMMANDS {
            let help = subcommand_help_text(subcommand.name);
            for (blind, note, label) in [
                (
                    config_blind_reason(subcommand.name).is_some(),
                    CONFIG_BLIND_NOTE,
                    "--config",
                ),
                (
                    agent_blind_reason(subcommand.name).is_some(),
                    AGENT_BLIND_NOTE,
                    "--agent",
                ),
                (
                    json_blind_reason(subcommand.name).is_some(),
                    JSON_BLIND_NOTE,
                    "--json",
                ),
            ] {
                assert_eq!(
                    help.contains(note),
                    blind,
                    "{}: help and the {label} refusal disagree",
                    subcommand.name
                );
            }
        }
    }

    /// The subcommand's own parse still wins: `--help` still helps, a bad local option is named.
    #[test]
    fn the_subcommands_own_parse_wins_over_the_config_refusal() {
        let outcome = parse(
            ["--config", "x.yaml", "init", "--help"]
                .iter()
                .map(OsString::from),
        )
        .ok()
        .expect("help is not an error");
        assert!(matches!(outcome, ParseResult::Exit(0)));

        let error = parse_args(&["--config", "x.yaml", "init", "--nonsense"]).expect_err("local");
        assert!(error.contains("--nonsense"), "{error}");
    }

    /// The bare form is gone, and its removal names the exact replacement.
    #[test]
    fn the_bare_command_form_is_refused_with_the_run_replacement() {
        let error = parse_args(&["git status"]).expect_err("bare command");
        assert!(error.contains("unknown subcommand 'git status'"), "{error}");
        assert!(error.contains("herdr-run run 'git status'"), "{error}");

        let error = parse_args(&["release-agent", "git status"]).expect_err("bare agent form");
        assert!(
            error.contains("herdr-run --agent release-agent run 'git status'"),
            "{error}"
        );

        let error = parse_args(&["stauts"]).expect_err("typo");
        assert!(error.contains("unknown subcommand 'stauts'"), "{error}");
        assert!(error.contains("Subcommands: "), "{error}");
        // A single word is a mistyped subcommand, not a command line; suggesting `run 'stauts'`
        // would send the reader off to debug the wrong thing.
        assert!(!error.contains("run 'stauts'"), "{error}");
    }

    #[test]
    fn run_needs_exactly_one_quoted_command() {
        let error = parse_args(&["run"]).expect_err("no command");
        assert!(error.contains("needs a command to run"), "{error}");
        let error = parse_args(&["run", "git", "status"]).expect_err("loose words");
        assert!(error.contains("ONE quoted argument"), "{error}");
        assert!(error.contains("herdr-run run 'git status'"), "{error}");
    }

    #[test]
    fn a_double_dash_hands_the_rest_to_the_command() {
        let invocation = parse_args(&["run", "--dry-run", "--", "git --help"]).expect("parse");
        assert_eq!(
            invocation.selected,
            Selected::Run(RunOptions {
                command: "git --help".to_owned(),
                dry_run: true,
                ..RunOptions::default()
            })
        );
        let invocation = parse_args(&["check", "--", "--version"]).expect("parse");
        assert_eq!(invocation.selected, Selected::Check("--version".to_owned()));
    }

    #[test]
    fn invalid_cli_timeouts_are_rejected() {
        for value in ["nan", "inf", "-1", "1e300"] {
            assert!(parse_args(&["run", "--timeout", value, "git status"]).is_err());
        }
    }

    #[test]
    fn option_values_cannot_be_stolen_by_help_or_version() {
        assert!(parse_args(&["--agent", "--help", "run", "x"]).is_err());
        assert!(parse_args(&["run", "--cwd", "--version", "x"]).is_err());
    }

    #[test]
    fn every_subcommand_is_reachable_and_uniquely_named() {
        assert_eq!(
            subcommand_names(),
            "check, config, init, net-doctor, quickstart, reap, run, status, target, userguide"
        );
        for subcommand in SUBCOMMANDS {
            assert!(
                find_subcommand(subcommand.name).is_some(),
                "{}",
                subcommand.name
            );
            assert!(!subcommand.summary.is_empty(), "{}", subcommand.name);
        }
    }

    /// Neither help level may document the other's options.
    ///
    /// The complaint that produced this surface was that every flag was global whether or not it
    /// meant anything to the thing being invoked, and help that lists the wrong options is how
    /// that state of affairs comes back without anybody deciding to bring it back.
    /// Return the contiguous indented lines that follow the first line starting with `header`.
    ///
    /// Only the option BLOCK is inspected: an example line naming a global option is teaching
    /// where that option goes, which is the opposite of the mixing being guarded against.
    fn option_block(text: &str, header: &str) -> String {
        text.lines()
            .skip_while(|line| !line.starts_with(header))
            .skip(1)
            .take_while(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn each_help_level_documents_exactly_its_own_options() {
        let top = help_text();
        for subcommand in SUBCOMMANDS {
            assert!(
                top.contains(subcommand.name) && top.contains(subcommand.summary),
                "top-level help omits {}",
                subcommand.name
            );
        }
        for global in ["--config", "--agent", "--json", "--version", "--help"] {
            assert!(top.contains(global), "top-level help omits {global}");
        }
        let top_options = option_block(&top, "global options");
        for global in ["--config", "--agent", "--json", "--version"] {
            assert!(
                top_options.contains(global),
                "the global option block omits {global}"
            );
        }
        for local in [
            "--cwd",
            "--timeout",
            "--wait-ready",
            "--force",
            "--no-cache",
            "--dry-run",
        ] {
            assert!(
                !top_options.contains(local),
                "the global option block documents the subcommand option {local}"
            );
        }

        let run = option_block(&subcommand_help_text("run"), "options:");
        for local in [
            "--cwd",
            "--timeout",
            "--wait-ready",
            "--no-cache",
            "--dry-run",
        ] {
            assert!(run.contains(local), "'run --help' omits {local}");
        }
        for global in ["--config", "--agent", "--json", "--version"] {
            assert!(
                !run.contains(global),
                "'run --help' documents the global option {global}"
            );
        }
        let init = option_block(&subcommand_help_text("init"), "options:");
        assert!(
            init.contains("--force") && init.contains("--upgrade"),
            "{init}"
        );
        assert!(option_block(&subcommand_help_text("target"), "options:").contains("--no-cache"));
        for bare in [
            "check",
            "config",
            "reap",
            "net-doctor",
            "quickstart",
            "userguide",
            "status",
        ] {
            let text = option_block(&subcommand_help_text(bare), "options:");
            assert!(text.contains("-h, --help"), "'{bare} --help' omits --help");
            for local in ["--cwd", "--timeout", "--dry-run", "--force", "--no-cache"] {
                assert!(
                    !text.contains(local),
                    "'{bare} --help' documents {local}, which it does not accept"
                );
            }
        }
    }

    #[test]
    fn init_takes_force_or_upgrade_but_not_both() {
        for (arguments, force, upgrade) in [
            (vec!["init"], false, false),
            (vec!["init", "--force"], true, false),
            (vec!["init", "--upgrade"], false, true),
            (vec!["--json", "init", "--upgrade"], false, true),
        ] {
            let invocation = parse_args(&arguments).expect("accepted");
            assert_eq!(
                invocation.selected,
                Selected::Init { force, upgrade },
                "{arguments:?}"
            );
        }
        for arguments in [
            vec!["init", "--force", "--upgrade"],
            vec!["init", "--upgrade", "--force"],
        ] {
            assert_eq!(
                parse_args(&arguments).expect_err("both"),
                "init: --force and --upgrade cannot be combined: --force writes a fresh file over the existing one, and --upgrade edits the existing one in place"
            );
        }
        let error = parse_args(&["--upgrade", "init"]).expect_err("before the subcommand");
        assert!(error.contains("put it AFTER the subcommand"), "{error}");
    }

    /// The top-level help must say what each of the two documentation commands is FOR.
    #[test]
    fn help_distinguishes_the_quickstart_from_the_userguide() {
        let top = help_text();
        assert!(top.contains("Two documentation commands"), "{top}");
        assert!(top.contains("one screen"), "{top}");
        assert!(top.contains("the complete reference"), "{top}");
    }

    /// Each documentation subcommand prints ITS OWN document.
    #[test]
    fn each_documentation_subcommand_prints_its_own_document() {
        assert!(document_for(&Selected::Quickstart)
            .expect("quickstart prints a document")
            .starts_with("# herdr-run — quickstart\n"));
        assert!(document_for(&Selected::Userguide)
            .expect("userguide prints a document")
            .starts_with("# herdr-run — user guide\n"));
        assert_eq!(document_for(&Selected::Config), None);
        assert_eq!(document_for(&Selected::Reap), None);
    }

    /// The two documents are different documents, and the short one is the short one.
    #[test]
    fn the_quickstart_is_a_shorter_amended_version_of_the_guide() {
        assert_ne!(crate::QUICKSTART, crate::USER_GUIDE);
        assert!(
            crate::QUICKSTART.lines().count() * 2 < crate::USER_GUIDE.lines().count(),
            "the quickstart is {} lines against the guide's {}; it is supposed to be much shorter",
            crate::QUICKSTART.lines().count(),
            crate::USER_GUIDE.lines().count()
        );
        assert!(crate::QUICKSTART.contains("herdr-run userguide"));
    }

    #[test]
    fn default_command_cwd_is_the_callers_current_directory() {
        assert_eq!(
            resolved_cwd(&Config::default(), None).unwrap(),
            std::env::current_dir().unwrap()
        );
    }

    #[test]
    fn vanished_caller_cwd_is_a_typed_config_error() {
        let error = resolved_cwd_with(&Config::default(), None, || {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "caller directory was removed",
            ))
        })
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
        assert_eq!(error.exit_code(), 78);
        assert!(error
            .message()
            .contains("cannot determine current directory"));
    }

    #[test]
    fn explicit_empty_cwd_means_the_project_root() {
        let config = Config {
            project_root: "/project".to_owned(),
            cwd: Some("configured".to_owned()),
            ..Config::default()
        };
        assert_eq!(
            resolved_cwd(&config, Some("")).unwrap(),
            Path::new("/project")
        );
    }

    #[test]
    fn golden_agent_resolution() {
        let document: Value =
            serde_json::from_str(include_str!("../testdata/text_cases.json")).expect("corpus");
        for row in document["agent"].as_array().expect("agent rows") {
            let environment = row[0].as_object().expect("environment");
            let lookup = |key: &str| {
                environment
                    .get(key)
                    .map(|value| OsString::from(value.as_str().expect("string")))
            };
            let outcome = match resolve_agent_from(row[1].as_str(), lookup) {
                Ok(agent) => json!({ "agent": agent }),
                Err(error) => json!({ "error": error.message() }),
            };
            assert_eq!(outcome, row[2], "{row:?}");
        }
    }

    /// The subcommand and every argument are whatever was typed. `check` used to print them raw,
    /// so a right-to-left override in the subcommand reordered the line on the terminal and a
    /// zero-width space in an argument could not be seen at all.
    #[test]
    fn check_shows_an_invisible_or_bidirectional_character_escaped() {
        let config = Config::default();
        let shown = |command: &str| {
            let admission = admit(command, &config).expect(command);
            check_text(
                &admission.program,
                admission.subcommand.as_deref(),
                admission.rendered(),
            )
        };
        assert_eq!(
            shown("git '\u{202e}status' 'a\u{200b}b' caf\u{e9}"),
            "ALLOWED: program=git subcommand=\"\\u{202e}status\"\n\
             rendered: \"git '\\u{202e}status' 'a\\u{200b}b' 'caf\\u{e9}'\"\n"
        );
        assert_eq!(
            shown("git commit -m caf\u{e9}"),
            "ALLOWED: program=git subcommand=commit\nrendered: git commit -m 'caf\u{e9}'\n"
        );
    }

    /// What `check` prints for an admitted command, unsafe characters shown escaped.
    #[test]
    fn golden_check_text() {
        let document: Value =
            serde_json::from_str(include_str!("../testdata/text_cases.json")).expect("corpus");
        for row in document["check_text"].as_array().expect("check_text rows") {
            let text = check_text(
                row[0].as_str().expect("program"),
                row[1].as_str(),
                row[2].as_str().expect("rendered"),
            );
            assert_eq!(text, row[3].as_str().expect("expected text"), "{row:?}");
        }
    }

    #[test]
    fn net_doctor_failures_have_one_verdict_and_metadata_shape() {
        for kind in [
            ErrorKind::Config,
            ErrorKind::Unavailable,
            ErrorKind::Busy,
            ErrorKind::Timeout,
            ErrorKind::Other,
        ] {
            assert!(!error_verdict(kind).is_empty());
            let fields = net_doctor_error_fields(Some(17));
            assert_eq!(fields.get("net_doctor"), Some(&json!(true)));
            assert_eq!(fields.get("direct_exit_code"), Some(&json!(17)));
        }

        let refusal = net_doctor_error_fields(None);
        assert_eq!(refusal.get("net_doctor"), Some(&json!(true)));
        assert!(!refusal.contains_key("direct_exit_code"));
        assert_eq!(error_verdict(ErrorKind::Refused), "REFUSED");
    }

    /// The scope note must lead, and must not be phrased as being about agents alone.
    #[test]
    fn net_doctor_announces_its_narrow_scope_before_probing() {
        assert!(NET_DOCTOR_SCOPE.starts_with("This is a smoke test for ONE scenario"));
        assert!(NET_DOCTOR_SCOPE.contains("not a health check of this command as a whole"));
    }
}
