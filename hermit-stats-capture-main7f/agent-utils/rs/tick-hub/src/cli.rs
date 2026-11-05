//! Command-line frontend for the tick engine.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::cadence::{load_fired_state, persist_fired_state};
use crate::engine::run_tick_with_emit;
use crate::io::{config_from_json, config_from_yaml, config_to_json, config_to_yaml};
use crate::model::{TickConfig, EVERY_TICK};
use crate::probes::{wall_clock_now, GlobFileAgeProbe, SubprocessGateRunner};
use crate::state::{state_lines, OpsState};
use crate::{PROG, USER_GUIDE, VERSION};

/// Environment variable overriding the fired-state path.
pub const STATE_FILE_ENV: &str = "TICK_HUB_STATE";
/// Default fired-state path, relative to the current directory.
pub const DEFAULT_FIRED_STATE: &str = ".tick-hub/state";

#[derive(Clone, Copy)]
struct Palette {
    enabled: bool,
}

impl Palette {
    fn wrap(self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn bold(self, text: &str) -> String {
        self.wrap("1", text)
    }

    fn dim(self, text: &str) -> String {
        self.wrap("2", text)
    }

    fn yellow(self, text: &str) -> String {
        self.wrap("33", text)
    }

    fn cyan(self, text: &str) -> String {
        self.wrap("36", text)
    }
}

fn banner(c: Palette) -> String {
    format!(
        "{} {}\n\
A single scheduled tick that funnels many recurring responsibilities, each on its own\n\
cadence, and emits machine-readable HEALTH/ACTION/NOTE/ERROR lines for a coordinator or\n\
automation to dispatch. Reminders, gates, and health checks are all caller config.",
        c.bold(PROG),
        c.dim(&format!("v{VERSION}"))
    )
}

fn root_help(c: Palette) -> String {
    format!(
        "usage: {PROG} [-h] [--version] [--userguide] <command> ...\n\n\
{}\n\n\
positional arguments:\n\
  <command>\n\
    tick        run one tick (emit HEALTH/ACTION/NOTE/ERROR lines)\n\
    state       validate + show the ops-state's own lines\n\
    list        list the reminders + health checks\n\
    json        re-emit the config as canonical JSON\n\
    yaml        re-emit the config as YAML\n\
    quickstart  print a self-contained getting-started guide\n\n\
options:\n\
  -h, --help    show this help message and exit\n\
  --version     show program's version number and exit\n\
  --userguide   print the full embedded user guide (the complete reference)\n\
                and exit\n\n\
{}\n\
  {}                          {}\n\
  {}              {}\n\
  {}      {}\n\
  {}              {}\n\
  {}             {}\n\n\
{}\n\
{}",
        banner(c),
        c.bold("examples"),
        c.cyan("tick-hub quickstart"),
        c.dim("# get started (model + runnable demo)"),
        c.cyan("tick-hub tick --config ops.yaml"),
        c.dim("# one tick (dry-run: no state write)"),
        c.cyan("tick-hub tick --config ops.yaml --flush"),
        c.dim("# ...and persist the fired-state"),
        c.cyan("tick-hub list --config ops.yaml"),
        c.dim("# reminders + health checks"),
        c.cyan("tick-hub state --state host.yaml"),
        c.dim("# validate + show the ops-state lines"),
        c.dim("The fired-state (cadence epochs + reserved retry diagnostics) lives at ./.tick-hub/state by"),
        c.dim("default (override with --fired-state or $TICK_HUB_STATE); it is written only on --flush.")
    )
}

fn quickstart(c: Palette) -> String {
    format!(
        "{}\n\n\
{}  {}\n\
  A \"tick\" is one heartbeat (a cron job, a coordinator loop, or a systemd timer). On each\n\
  tick the hub checks every due reminder and prints a stable, line-oriented report. One loop\n\
  can therefore carry many recurring responsibilities with independent cadences.\n\n\
{}\n\
  cargo install tick-hub\n\n\
{}  {}\n\
  Save as ops.yaml:\n\
  reminders:\n\
    - name: git_sync\n\
      cadence_secs: 21600\n\
      emit: {{kind: action, skill: git-sync, title: \"fetch origin and reconcile\"}}\n\
    - name: backlog\n\
      gate: {{cmd: \"echo count=42\", when: always, capture: true}}\n\
      emit:\n\
        kind: action\n\
        skill: backlog-triage\n\
        fields: {{threshold: \"20\"}}\n\
        title: \"backlog has {{count}} ready items (threshold {{threshold}})\"\n\n\
{}\n\
  {}          {}\n\
  {}  {}\n\n\
{}  {}\n\
  HEALTH: <check> <ok|stale|missing> age_secs=<N|NA> threshold_secs=<N> detail=\"...\"\n\
  ACTION: <handler> [key=value ...] title=\"...\"\n\
  NOTE:   <free text>\n\
  ERROR:  <text>\n\n\
{}\n\
  Last-fired epochs and reserved __tick_hub_internal__.* retry diagnostics share\n\
  ./.tick-hub/state by default. The file is written\n\
  only with {}; a dry run mutates nothing.\n\n\
{}  0 = tick ran | 2 = bad usage / bad config or state file",
        banner(c),
        c.bold("The idea"),
        c.dim("- one heartbeat, many cadenced reminders, machine-readable output"),
        c.bold("1. Install"),
        c.bold("2. Write a reminder set (YAML)"),
        c.dim("- JSON works too"),
        c.bold("3. Run a tick"),
        c.cyan("tick-hub tick --config ops.yaml"),
        c.dim("# dry-run: print without persisting state"),
        c.cyan("tick-hub tick --config ops.yaml --flush"),
        c.dim("# persist per-reminder last-fired epochs"),
        c.bold("The output contract"),
        c.dim("(parse each independent line by its leading token)"),
        c.bold("Cadence and state"),
        c.cyan("--flush"),
        c.bold("Exit codes")
    )
}

fn command_help(command: &str) -> String {
    match command {
        "tick" => format!(
            "usage: {PROG} tick --config FILE [--state FILE] [--fired-state FILE] [--now EPOCH]\n\
                    [--current-tick-min N] [--flush] [--report-pending] [--no-header]\n\n\
options:\n\
  -h, --help            show this help message and exit\n\
  --config FILE         reminder-set config; .yaml/.yml load as YAML, else JSON\n\
  --state FILE          optional per-host ops-state YAML\n\
  --fired-state FILE    cadence and reserved retry-diagnostic state file\n\
  --now EPOCH           override the clock for deterministic runs\n\
  --current-tick-min N  actually-running tick cadence in minutes\n\
  --flush               persist the advanced fired-state\n\
  --report-pending      print CLEAN/SUPPRESSED verdicts for due reminders\n\
  --no-header           suppress the explanatory stderr banner"
        ),
        "state" => format!(
            "usage: {PROG} state --state FILE [--current-tick-min N]\n\n\
options:\n  -h, --help            show this help message and exit\n\
  --state FILE          ops-state YAML file\n\
  --current-tick-min N  actually-running tick cadence in minutes"
        ),
        "list" | "json" | "yaml" => format!(
            "usage: {PROG} {command} --config FILE\n\n\
options:\n  -h, --help     show this help message and exit\n\
  --config FILE  config file; .yaml/.yml load as YAML, else JSON"
        ),
        "quickstart" => format!(
            "usage: {PROG} quickstart [-h]\n\noptions:\n  -h, --help  show this help message and exit"
        ),
        _ => String::new(),
    }
}

#[derive(Default)]
struct TickArgs {
    config: Option<String>,
    state: Option<String>,
    fired_state: Option<String>,
    now: Option<i64>,
    current_tick_min: Option<i64>,
    flush: bool,
    report_pending: bool,
    no_header: bool,
}

fn value_after(args: &[String], index: &mut usize, option: &str) -> Result<String, String> {
    *index += 1;
    let value = args
        .get(*index)
        .ok_or_else(|| format!("argument {option}: expected one argument"))?;
    if value.starts_with('-') && !looks_like_negative_number(value) {
        return Err(format!("argument {option}: expected one argument"));
    }
    Ok(value.clone())
}

fn looks_like_negative_number(value: &str) -> bool {
    let Some(unsigned) = value.strip_prefix('-') else {
        return false;
    };
    if !unsigned.is_empty() && unsigned.bytes().all(|byte| byte.is_ascii_digit()) {
        return true;
    }
    let Some((whole, fraction)) = unsigned.split_once('.') else {
        return false;
    };
    whole.bytes().all(|byte| byte.is_ascii_digit())
        && !fraction.is_empty()
        && fraction.bytes().all(|byte| byte.is_ascii_digit())
}

fn split_option<'a>(arg: &'a str, option: &str) -> Option<Option<&'a str>> {
    if arg == option {
        Some(None)
    } else {
        arg.strip_prefix(&format!("{option}=")).map(Some)
    }
}

fn option_value(
    args: &[String],
    index: &mut usize,
    arg: &str,
    option: &str,
) -> Option<Result<String, String>> {
    split_option(arg, option).map(|inline| match inline {
        Some(value) => Ok(value.to_string()),
        None => value_after(args, index, option),
    })
}

fn parse_i64(value: String, option: &str) -> Result<i64, String> {
    value
        .parse()
        .map_err(|_| format!("argument {option}: invalid int value: '{value}'"))
}

fn parse_nonnegative_i64(value: String, option: &str) -> Result<i64, String> {
    let value = parse_i64(value, option)?;
    if value < 0 {
        return Err(format!("argument {option}: value must be non-negative"));
    }
    Ok(value)
}

fn parse_positive_i64(value: String, option: &str) -> Result<i64, String> {
    let value = parse_i64(value, option)?;
    if value <= 0 {
        return Err(format!("argument {option}: value must be positive"));
    }
    Ok(value)
}

fn parse_tick(args: &[String]) -> Result<TickArgs, String> {
    let mut out = TickArgs::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(value) = option_value(args, &mut index, arg, "--config") {
            out.config = Some(value?);
        } else if let Some(value) = option_value(args, &mut index, arg, "--state") {
            out.state = Some(value?);
        } else if let Some(value) = option_value(args, &mut index, arg, "--fired-state") {
            out.fired_state = Some(value?);
        } else if let Some(value) = option_value(args, &mut index, arg, "--now") {
            out.now = Some(parse_nonnegative_i64(value?, "--now")?);
        } else if let Some(value) = option_value(args, &mut index, arg, "--current-tick-min") {
            out.current_tick_min = Some(parse_positive_i64(value?, "--current-tick-min")?);
        } else if arg == "--flush" {
            out.flush = true;
        } else if arg == "--report-pending" {
            out.report_pending = true;
        } else if arg == "--no-header" {
            out.no_header = true;
        } else {
            return Err(format!("unrecognized arguments: {arg}"));
        }
        index += 1;
    }
    if out.config.is_none() {
        return Err("the following arguments are required: --config".into());
    }
    Ok(out)
}

fn parse_named_options(
    args: &[String],
    required: &str,
    allow_current_tick: bool,
) -> Result<(String, Option<i64>), String> {
    let mut path = None;
    let mut current = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(value) = option_value(args, &mut index, arg, required) {
            path = Some(value?);
        } else if allow_current_tick {
            if let Some(value) = option_value(args, &mut index, arg, "--current-tick-min") {
                current = Some(parse_positive_i64(value?, "--current-tick-min")?);
            } else {
                return Err(format!("unrecognized arguments: {arg}"));
            }
        } else {
            return Err(format!("unrecognized arguments: {arg}"));
        }
        index += 1;
    }
    path.map(|path| (path, current))
        .ok_or_else(|| format!("the following arguments are required: {required}"))
}

fn load_config(path: &Path) -> Result<TickConfig, String> {
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let is_yaml = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("yaml") || extension.eq_ignore_ascii_case("yml")
        });
    if is_yaml {
        config_from_yaml(&text).map_err(|error| error.to_string())
    } else {
        config_from_json(&text).map_err(|error| error.to_string())
    }
}

fn fired_path(argument: Option<&str>) -> PathBuf {
    argument
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os(STATE_FILE_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from(DEFAULT_FIRED_STATE))
}

/// Render a compact inventory of reminders and health checks.
pub fn render_list(config: &TickConfig, color: bool) -> String {
    let c = Palette { enabled: color };
    let mut lines = Vec::new();
    if config.reminders.is_empty() {
        lines.push(c.dim("(no reminders)"));
    } else {
        lines.push(c.bold("reminders:"));
        let width = config
            .reminders
            .iter()
            .map(|reminder| reminder.name.chars().count())
            .max()
            .unwrap_or(0);
        for reminder in &config.reminders {
            let cadence = if reminder.cadence_secs == EVERY_TICK {
                "every-tick".to_string()
            } else {
                format!("{}s", reminder.cadence_secs)
            };
            let gate = reminder.gate.as_ref().map_or_else(String::new, |gate| {
                c.dim(&format!(
                    " gate[{}{}]",
                    gate.when.as_str(),
                    if gate.capture { ",capture" } else { "" }
                ))
            });
            let flags = if reminder.requires_flags.is_empty() {
                String::new()
            } else {
                c.dim(&format!(" needs={}", reminder.requires_flags.join(",")))
            };
            let target = if reminder.emit.skill.is_empty() {
                &reminder.emit.title
            } else {
                &reminder.emit.skill
            };
            lines.push(format!(
                "  {}  {} {} {}{}{}",
                c.bold(&format!("{:<width$}", reminder.name)),
                c.yellow(&format!("[{}]", reminder.emit.kind.as_str())),
                c.cyan(&cadence),
                target,
                gate,
                flags
            ));
        }
    }
    if !config.health_checks.is_empty() {
        lines.push(c.bold("health checks:"));
        for check in &config.health_checks {
            lines.push(format!(
                "  {}  {} {}",
                c.bold(&check.name),
                c.dim(&check.glob),
                c.cyan(&format!("threshold={}s", check.threshold_secs))
            ));
        }
    }
    lines.join("\n")
}

fn report_parse_error(stderr: &mut dyn Write, command: &str, error: &str) -> i32 {
    let _ = writeln!(stderr, "usage: {PROG} {command} ...");
    let _ = writeln!(stderr, "{PROG}: error: {error}");
    2
}

fn run_tick_command(
    args: TickArgs,
    c: Palette,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let config_path = Path::new(args.config.as_deref().expect("validated by parser"));
    let config = match load_config(config_path) {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(stderr, "{PROG}: {error}");
            return 2;
        }
    };
    let (state, state_note) = match args.state.as_deref() {
        None => (
            OpsState::default(),
            Some("no --state file given; using the default enabled ops-state".to_string()),
        ),
        Some(path) if !Path::new(path).is_file() => (
            OpsState::default(),
            Some(format!(
                "ops-state file {path} not found; using the default enabled ops-state"
            )),
        ),
        Some(path) => match OpsState::load(path) {
            Ok(state) => (state, None),
            Err(error) => {
                let _ = writeln!(stderr, "{PROG}: invalid ops-state {path}: {error}");
                return 2;
            }
        },
    };
    let path = fired_path(args.fired_state.as_deref());
    let fired = load_fired_state(&path);
    if !args.no_header {
        let _ = writeln!(stderr, "{}", banner(c));
        if let Some(note) = state_note {
            let _ = writeln!(stderr, "{PROG}: {note}");
        }
    }
    let result = {
        let mut emit_line = |line: &str| {
            writeln!(stdout, "{line}")?;
            stdout.flush()
        };
        run_tick_with_emit(
            &config,
            &state,
            args.now.unwrap_or_else(wall_clock_now),
            &fired,
            &SubprocessGateRunner::default(),
            &GlobFileAgeProbe,
            args.current_tick_min,
            args.report_pending,
            &mut emit_line,
        )
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = writeln!(stderr, "{PROG}: could not write tick report: {error}");
            return 1;
        }
    };
    if args.flush {
        if let Err(error) = persist_fired_state(&path, &result.fired) {
            let _ = writeln!(
                stderr,
                "{PROG}: cannot persist fired-state to {}: {error}",
                path.display()
            );
            return 2;
        }
        let _ = writeln!(
            stderr,
            "{PROG}: fired-state persisted to {}",
            path.display()
        );
    } else {
        let _ = writeln!(
            stderr,
            "{PROG}: dry-run (fired-state NOT persisted; pass --flush to persist)"
        );
    }
    0
}

fn run_inner(args: &[String], color: bool, stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let c = Palette { enabled: color };
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help") {
        let _ = writeln!(stdout, "{}", root_help(c));
        return 0;
    }
    if args[0] == "--version" {
        let _ = writeln!(stdout, "{PROG} {VERSION}");
        return 0;
    }
    if args[0] == "--userguide" {
        let _ = write!(stdout, "{USER_GUIDE}");
        return 0;
    }
    let command = args[0].as_str();
    let rest = &args[1..];
    if rest
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
        && matches!(
            command,
            "tick" | "state" | "list" | "json" | "yaml" | "quickstart"
        )
    {
        let _ = writeln!(stdout, "{}", command_help(command));
        return 0;
    }
    match command {
        "quickstart" => {
            if !rest.is_empty() {
                return report_parse_error(
                    stderr,
                    command,
                    &format!("unrecognized arguments: {}", rest.join(" ")),
                );
            }
            let _ = writeln!(stdout, "{}", quickstart(c));
            0
        }
        "tick" => match parse_tick(rest) {
            Ok(parsed) => run_tick_command(parsed, c, stdout, stderr),
            Err(error) => report_parse_error(stderr, command, &error),
        },
        "state" => match parse_named_options(rest, "--state", true) {
            Err(error) => report_parse_error(stderr, command, &error),
            Ok((path, current)) => match OpsState::load(&path) {
                Err(error) => {
                    let _ = writeln!(stderr, "{PROG}: invalid ops-state {path}: {error}");
                    2
                }
                Ok(state) => {
                    for line in state_lines(&state, current) {
                        let _ = writeln!(stdout, "{line}");
                    }
                    0
                }
            },
        },
        "list" | "json" | "yaml" => match parse_named_options(rest, "--config", false) {
            Err(error) => report_parse_error(stderr, command, &error),
            Ok((path, _)) => match load_config(Path::new(&path)) {
                Err(error) => {
                    let _ = writeln!(stderr, "{PROG}: {error}");
                    2
                }
                Ok(config) => {
                    match command {
                        "list" => {
                            let _ = writeln!(stdout, "{}", render_list(&config, color));
                        }
                        "json" => match config_to_json(&config) {
                            Ok(text) => {
                                let _ = writeln!(stdout, "{text}");
                            }
                            Err(error) => {
                                let _ = writeln!(stderr, "{PROG}: {error}");
                                return 2;
                            }
                        },
                        "yaml" => match config_to_yaml(&config) {
                            Ok(text) => {
                                let _ = write!(stdout, "{text}");
                            }
                            Err(error) => {
                                let _ = writeln!(stderr, "{PROG}: {error}");
                                return 2;
                            }
                        },
                        _ => unreachable!(),
                    }
                    0
                }
            },
        },
        _ => report_parse_error(
            stderr,
            "",
            &format!("argument <command>: invalid choice: '{command}'"),
        ),
    }
}

/// Run the CLI with explicit UTF-8 arguments and streams. Color is disabled for deterministic
/// embedding and tests.
pub fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    run_inner(args, false, stdout, stderr)
}

/// Run from the current process environment, enabling ANSI styling only for an interactive stdout.
pub fn run_from_env() -> i32 {
    let args: Vec<String> = env::args_os()
        .skip(1)
        .map(|argument: OsString| argument.to_string_lossy().into_owned())
        .collect();
    let color = env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal();
    run_inner(&args, color, &mut io::stdout(), &mut io::stderr())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FailedWriter;

    impl Write for FailedWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader closed"))
        }
    }

    fn invoke(args: &[&str]) -> (i32, String, String) {
        let args = args
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(&args, &mut stdout, &mut stderr);
        (
            code,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    fn temp_file(name: &str, text: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "tick-hub-cli-{}-{nonce}-{name}",
            std::process::id()
        ));
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn version_and_userguide_are_standalone() {
        let (_, version, _) = invoke(&["--version"]);
        assert_eq!(version, format!("tick-hub {VERSION}\n"));
        let (_, guide, _) = invoke(&["--userguide"]);
        assert_eq!(guide, USER_GUIDE);
    }

    #[test]
    fn list_and_json_load_yaml() {
        let path = temp_file(
            "config.yaml",
            "reminders:\n- name: sync\n  emit: {skill: git-sync, title: sync now}\n",
        );
        let path = path.to_string_lossy();
        let (code, list, error) = invoke(&["list", "--config", &path]);
        assert_eq!(code, 0, "{error}");
        assert!(list.contains("sync  [action] every-tick git-sync"));
        let (code, json, error) = invoke(&["json", "--config", &path]);
        assert_eq!(code, 0, "{error}");
        assert!(json.contains("\"description\": \"\""));
        let _ = fs::remove_file(path.as_ref());
    }

    #[test]
    fn tick_is_dry_by_default_and_uses_fallback_state() {
        let path = temp_file(
            "config.json",
            r#"{"reminders":[{"name":"r","emit":{"kind":"note","title":"hello"}}]}"#,
        );
        let fired = path.with_extension("state");
        let path_text = path.to_string_lossy();
        let fired_text = fired.to_string_lossy();
        let (code, stdout, stderr) = invoke(&[
            "tick",
            "--config",
            &path_text,
            "--fired-state",
            &fired_text,
            "--now",
            "10",
            "--no-header",
        ]);
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.contains("NOTE: hello\n"));
        assert!(stderr.contains("dry-run"));
        assert!(!fired.exists());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn report_pending_is_compatible_with_flush_and_cadence() {
        let config = temp_file(
            "report-pending.json",
            r#"{"reminders":[{"name":"quiet","cadence_secs":3600,"gate":{"cmd":"true","when":"failure"},"emit":{"skill":"warn","title":"problem"}}]}"#,
        );
        let fired = config.with_extension("state");
        let config_text = config.to_string_lossy();
        let fired_text = fired.to_string_lossy();
        let (code, stdout, stderr) = invoke(&[
            "tick",
            "--config",
            &config_text,
            "--fired-state",
            &fired_text,
            "--now",
            "1000",
            "--flush",
            "--report-pending",
            "--no-header",
        ]);
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.contains("CLEAN: quiet ran and found nothing to report\n"));
        assert!(fs::read_to_string(&fired).unwrap().contains("quiet=1000\n"));
        assert!(stderr.contains("persisted"));

        let (code, stdout, stderr) = invoke(&[
            "tick",
            "--config",
            &config_text,
            "--fired-state",
            &fired_text,
            "--now",
            "1001",
            "--report-pending",
            "--no-header",
        ]);
        assert_eq!(code, 0, "{stderr}");
        assert!(!stdout.contains("CLEAN: quiet"));
        let _ = fs::remove_file(config);
        let _ = fs::remove_file(fired);
    }

    #[test]
    fn failed_report_write_returns_nonzero_without_persisting_cadence() {
        let config = temp_file(
            "failed-writer.json",
            r#"{"reminders":[{"name":"due","emit":{"skill":"warn","title":"problem"}}]}"#,
        );
        let fired = config.with_extension("state");
        let args = [
            "tick",
            "--config",
            &config.to_string_lossy(),
            "--fired-state",
            &fired.to_string_lossy(),
            "--now",
            "1000",
            "--flush",
            "--report-pending",
            "--no-header",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let mut stdout = FailedWriter;
        let mut stderr = Vec::new();
        let code = run(&args, &mut stdout, &mut stderr);
        assert_eq!(code, 1);
        assert!(String::from_utf8(stderr)
            .unwrap()
            .contains("could not write tick report"));
        assert!(!fired.exists(), "a failed report must not consume cadence");
        let _ = fs::remove_file(config);
    }

    #[test]
    fn flush_failure_is_controlled_and_cleans_temporary_file() {
        let config = temp_file("flush-error.json", r#"{"reminders":[]}"#);
        let fired = config.with_extension("state-dir");
        fs::create_dir_all(&fired).unwrap();
        let config_text = config.to_string_lossy();
        let fired_text = fired.to_string_lossy();
        let (code, _, stderr) = invoke(&[
            "tick",
            "--config",
            &config_text,
            "--fired-state",
            &fired_text,
            "--now",
            "10",
            "--flush",
            "--no-header",
        ]);
        assert_eq!(code, 2);
        assert!(stderr.contains("cannot persist fired-state"));
        assert!(!fired.with_extension("state-dir.tmp").exists());
        let _ = fs::remove_file(config);
        let _ = fs::remove_dir_all(fired);
    }

    #[test]
    fn missing_required_option_is_usage_error() {
        let (code, _, stderr) = invoke(&["tick"]);
        assert_eq!(code, 2);
        assert!(stderr.contains("required: --config"));
    }

    #[test]
    fn clock_and_tick_cadence_enforce_domain_bounds() {
        let (code, _, stderr) = invoke(&["tick", "--config", "unused", "--now", "-1"]);
        assert_eq!(code, 2);
        assert!(stderr.contains("--now: value must be non-negative"));

        let (code, _, stderr) = invoke(&["state", "--state", "unused", "--current-tick-min", "0"]);
        assert_eq!(code, 2);
        assert!(stderr.contains("--current-tick-min: value must be positive"));
    }
}
