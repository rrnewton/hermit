//! Command-line entry point: argument parsing, help, and dispatch.

use crate::daemon;
use crate::history;
use crate::model::{Status, PROVIDERS};
use crate::paths::{cache_dir, process_env, CachePaths};
use crate::poll::{step, PollOptions};
use crate::report;
use crate::timefmt::{human_duration, parse_duration, rfc3339_utc};
use std::ffi::OsString;

/// Exit status for bad arguments.
pub const EXIT_USAGE: i32 = 64;
/// Exit status when a step could not run (cache directory unusable and similar).
pub const EXIT_SOFTWARE: i32 = 70;
/// Exit status when the daemon is already running.
pub const EXIT_BUSY: i32 = 75;

/// Default `--max-age` for `status`: a sample younger than this is reused instead of polling.
pub const DEFAULT_MAX_AGE: i64 = 120;

const HELP: &str = "\
agent-usage: plan usage, reset times and burn rate for the Claude Code and Codex CLIs

Reads how much of each subscription's plan windows (Claude's 5-hour session and weekly
windows, Codex's 5-hour and weekly windows) is used and when each resets, from the same
structured sources the CLIs' own /status screens use. No model call, no session, no tokens.
Every reading is appended to a history file, and each report adds burn rates over the last
15 minutes, 1 hour, 3 hours and 24 hours, plus a projection of when a window would fill.
Local token use is reported too, from Claude Code transcripts and Codex's thread totals,
including on hosts that have no plan limits (API keys, cloud providers, gateways).

USAGE
  agent-usage [status] [--json | --line] [--provider P] [--max-age DUR | --cached] [--no-tokens]
  agent-usage poll [--json] [--provider P]
  agent-usage daemon [--interval DUR] [--once] [--detach]
  agent-usage daemon status | stop
  agent-usage history [--provider P] [--since DUR] [--json]
  agent-usage path
  agent-usage quickstart | userguide | help | --help | -h | --version

SUBCOMMANDS
  status      (default) Report current meters, burn rates and local token windows. Polls a
              provider only when its newest sample is older than --max-age.
  poll        Read every selected provider now, append to the history, print the samples.
  daemon      Poll every --interval in the foreground until SIGTERM/SIGINT; one daemon per
              cache directory. `daemon status` and `daemon stop` manage a running one.
  history     Print stored samples.
  path        Print the cache directory.
  quickstart  One-screen introduction.   userguide  Full reference.

OPTIONS
  --json          Machine-readable output (status, poll, history, daemon status).
  --line          status: one line (meters, burn per hour, reset, 1 h tokens), for status
                  lines and agents that want the gist in few tokens.
  --provider P    claude, codex or all (default all). Repeatable.
  --max-age DUR   status: reuse a sample younger than DUR instead of polling. Default 120s.
                  0 always polls.
  --cached        status: never poll; report from the history only.
  --no-tokens     status: skip the Claude transcript scan.
  --interval DUR  daemon: polling interval. Default 15m; at least 60s.
  --once          daemon: poll once and exit (for cron or systemd timers).
  --detach        daemon: start in the background (new session, log in the cache
                  directory) and print its pid.
  --since DUR     history: how far back. Default 24h.
  DUR is seconds, or a number with s, m, h or d (90s, 15m, 3h, 1d).

FILES
  Cache directory: $AGENT_USAGE_DIR, else $XDG_CACHE_HOME/agent-usage, else
  ~/.cache/agent-usage. history.jsonl (samples), claude-transcripts.json (transcript index),
  daemon.pid, daemon.log. Claude's login: $CLAUDE_CONFIG_DIR/.credentials.json (default
  ~/.claude). Codex's home: $CODEX_HOME (default ~/.codex).

EXIT STATUS
  0 report printed (even when a provider's usage is unknown: read the status field), 64 bad
  arguments, 70 cache unusable, 75 daemon already running.

EXAMPLES
  agent-usage                       # human report, polling if older than 2 minutes
  agent-usage --json --cached       # what the daemon last saw, for scripts
  agent-usage daemon --detach       # keep a 15-minute history for burn rates
  agent-usage status --provider claude --max-age 0
";

/// Parsed command line.
#[derive(Debug, Clone, PartialEq)]
struct Args {
    command: String,
    sub: Option<String>,
    json: bool,
    line: bool,
    providers: Vec<&'static str>,
    max_age: Option<i64>,
    tokens: bool,
    interval: i64,
    once: bool,
    detach: bool,
    since: i64,
}

fn parse(args: Vec<String>) -> Result<Args, String> {
    let mut out = Args {
        command: "status".into(),
        sub: None,
        json: false,
        line: false,
        providers: Vec::new(),
        max_age: Some(DEFAULT_MAX_AGE),
        tokens: true,
        interval: daemon::DEFAULT_INTERVAL,
        once: false,
        detach: false,
        since: 86_400,
    };
    let mut it = args.into_iter().peekable();
    let mut positional = Vec::new();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--json" => out.json = true,
            "--line" => out.line = true,
            "--cached" => out.max_age = None,
            "--no-tokens" => out.tokens = false,
            "--once" => out.once = true,
            "--detach" => out.detach = true,
            "--provider" => {
                let v = value("--provider")?;
                match v.as_str() {
                    "all" => out.providers.extend(PROVIDERS),
                    "claude" => out.providers.push("claude"),
                    "codex" => out.providers.push("codex"),
                    _ => return Err(format!("unknown provider {v:?} (claude, codex or all)")),
                }
            }
            "--max-age" => {
                let v = value("--max-age")?;
                out.max_age =
                    Some(parse_duration(&v).ok_or_else(|| format!("bad --max-age {v:?}"))?);
            }
            "--interval" => {
                let v = value("--interval")?;
                out.interval = parse_duration(&v).ok_or_else(|| format!("bad --interval {v:?}"))?;
                if out.interval < daemon::MIN_INTERVAL {
                    return Err(format!(
                        "--interval must be at least {}s",
                        daemon::MIN_INTERVAL
                    ));
                }
            }
            "--since" => {
                let v = value("--since")?;
                out.since = parse_duration(&v).ok_or_else(|| format!("bad --since {v:?}"))?;
            }
            "-h" | "--help" => positional.push("help".to_string()),
            "--version" => positional.push("version".to_string()),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            _ => positional.push(arg),
        }
    }
    // One help page covers every subcommand, so `agent-usage daemon --help` and
    // `agent-usage help daemon` print it too.
    if positional.iter().any(|a| a == "help") {
        out.command = "help".into();
        return Ok(out);
    }
    let mut pos = positional.into_iter();
    if let Some(cmd) = pos.next() {
        out.command = cmd;
    }
    out.sub = pos.next();
    if let Some(extra) = pos.next() {
        return Err(format!("unexpected argument {extra:?}"));
    }
    if out.sub.is_some() && out.command != "daemon" && out.command != "help" {
        return Err(format!(
            "unexpected argument {:?}",
            out.sub.unwrap_or_default()
        ));
    }
    if out.providers.is_empty() {
        out.providers = PROVIDERS.to_vec();
    }
    out.providers.dedup();
    Ok(out)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Run the CLI with these arguments (program name excluded); returns the exit status.
pub fn main(args: impl Iterator<Item = OsString>) -> i32 {
    let args: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let args = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("agent-usage: {e}\nRun `agent-usage --help` for usage.");
            return EXIT_USAGE;
        }
    };
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("agent-usage: {e}");
            EXIT_SOFTWARE
        }
    }
}

fn run(args: &Args) -> Result<i32, String> {
    let env = process_env;
    match args.command.as_str() {
        "help" => {
            print!("{HELP}");
            return Ok(0);
        }
        "version" => {
            println!("agent-usage {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        "quickstart" => {
            print!("{}", crate::QUICKSTART);
            return Ok(0);
        }
        "userguide" => {
            print!("{}", crate::USER_GUIDE);
            return Ok(0);
        }
        _ => {}
    }
    let paths = CachePaths::new(cache_dir(&env)?);
    match args.command.as_str() {
        "path" => {
            println!("{}", paths.dir.display());
            Ok(0)
        }
        "status" => {
            let t = now();
            let opts = PollOptions {
                providers: args.providers.clone(),
                max_age: args.max_age,
                scan_transcripts: args.tokens,
            };
            let result = step(&paths, &env, &opts, t)?;
            let rep = report::build(
                &result.samples,
                result.index.as_ref(),
                &args.providers,
                &result.fresh,
                result.codex_retries.as_deref(),
                t,
            );
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rep).map_err(|e| e.to_string())?
                );
            } else if args.line {
                println!("{}", report::render_line(&rep));
            } else {
                print!("{}", report::render(&rep));
                if rep.providers.is_empty() {
                    println!("(--cached with an empty history: run `agent-usage poll`)");
                }
            }
            Ok(0)
        }
        "poll" => {
            let t = now();
            let opts = PollOptions {
                providers: args.providers.clone(),
                max_age: Some(0),
                scan_transcripts: false,
            };
            let result = step(&paths, &env, &opts, t)?;
            for s in result
                .samples
                .iter()
                .filter(|s| s.ts == t && result.fresh.contains(&s.provider.as_str()))
            {
                if args.json {
                    println!("{}", serde_json::to_string(s).map_err(|e| e.to_string())?);
                } else {
                    let what = match s.status {
                        Status::Ok => s
                            .meters
                            .iter()
                            .map(|m| format!("{} {:.0}%", m.label, m.used_pct))
                            .collect::<Vec<_>>()
                            .join(", "),
                        _ => s.detail.clone().unwrap_or_default(),
                    };
                    println!(
                        "{} {:?} in {}ms: {what}",
                        s.provider, s.status, s.elapsed_ms
                    );
                }
            }
            Ok(0)
        }
        "history" => {
            let t = now();
            let samples = history::read(&paths.history());
            for s in samples
                .iter()
                .filter(|s| s.ts >= t - args.since && args.providers.contains(&s.provider.as_str()))
            {
                if args.json {
                    println!("{}", serde_json::to_string(s).map_err(|e| e.to_string())?);
                } else {
                    let meters = s
                        .meters
                        .iter()
                        .map(|m| format!("{}={:.1}", m.id, m.used_pct))
                        .collect::<Vec<_>>()
                        .join(" ");
                    let tokens = s
                        .tokens_cumulative
                        .map(|t| format!(" tokens={}", t.total))
                        .unwrap_or_default();
                    println!(
                        "{} {:6} {:11} {meters}{tokens}",
                        rfc3339_utc(s.ts),
                        s.provider,
                        format!("{:?}", s.status).to_lowercase()
                    );
                }
            }
            Ok(0)
        }
        "daemon" => daemon_command(args, &paths, &env),
        other => {
            eprintln!(
                "agent-usage: unknown command {other:?}\nRun `agent-usage --help` for usage."
            );
            Ok(EXIT_USAGE)
        }
    }
}

fn daemon_command(args: &Args, paths: &CachePaths, env: crate::paths::Env) -> Result<i32, String> {
    match args.sub.as_deref() {
        None if args.detach => {
            let exe =
                std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;
            match daemon::detach(paths, &exe, args.interval) {
                Ok(pid) => {
                    println!(
                        "agent-usage daemon started: pid {pid}, every {}, log {}",
                        human_duration(args.interval),
                        paths.daemon_log().display()
                    );
                    Ok(0)
                }
                Err(e) => {
                    eprintln!("agent-usage daemon: {e}");
                    Ok(EXIT_BUSY)
                }
            }
        }
        None => daemon::run(paths, env, args.interval, args.once),
        Some("status") => {
            let pid = daemon::running_pid(paths);
            let samples = history::read(&paths.history());
            let last = samples.last().map(|s| s.ts);
            if args.json {
                let v = serde_json::json!({
                    "running": pid.is_some(),
                    "pid": pid,
                    "dir": paths.dir,
                    "last_sample": last,
                    "samples": samples.len(),
                });
                println!("{v}");
            } else {
                match pid {
                    Some(pid) => println!("running, pid {pid}"),
                    None => println!("not running"),
                }
                match last {
                    Some(ts) => println!(
                        "{} samples, newest {} ago, in {}",
                        samples.len(),
                        human_duration(now() - ts),
                        paths.history().display()
                    ),
                    None => println!("no samples in {}", paths.history().display()),
                }
            }
            Ok(0)
        }
        Some("stop") => match daemon::stop(paths)? {
            Some(pid) => {
                println!("sent SIGTERM to pid {pid}");
                Ok(0)
            }
            None => {
                println!("not running");
                Ok(0)
            }
        },
        Some(other) => {
            eprintln!("agent-usage: unknown daemon action {other:?} (status or stop)");
            Ok(EXIT_USAGE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn defaults_and_options() {
        let a = p(&[]).unwrap();
        assert_eq!(a.command, "status");
        assert_eq!(a.providers, PROVIDERS.to_vec());
        assert_eq!(a.max_age, Some(DEFAULT_MAX_AGE));
        let a = p(&["--json", "--provider", "claude", "--max-age", "0"]).unwrap();
        assert!(a.json);
        assert_eq!(a.providers, ["claude"]);
        assert_eq!(a.max_age, Some(0));
        assert!(p(&["--line"]).unwrap().line);
        let a = p(&["status", "--cached", "--no-tokens"]).unwrap();
        assert_eq!(a.max_age, None);
        assert!(!a.tokens);
        let a = p(&["daemon", "--interval", "5m", "--once"]).unwrap();
        assert_eq!(
            (a.command.as_str(), a.interval, a.once),
            ("daemon", 300, true)
        );
        let a = p(&["daemon", "stop"]).unwrap();
        assert_eq!(a.sub.as_deref(), Some("stop"));
        let a = p(&["history", "--since", "3h"]).unwrap();
        assert_eq!(a.since, 10_800);
        assert_eq!(p(&["--help"]).unwrap().command, "help");
        assert_eq!(p(&["daemon", "--help"]).unwrap().command, "help");
        assert_eq!(p(&["help", "status"]).unwrap().command, "help");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(p(&["--provider", "gemini"]).is_err());
        assert!(p(&["--interval", "10s", "daemon"]).is_err());
        assert!(p(&["--max-age"]).is_err());
        assert!(p(&["--frobnicate"]).is_err());
        assert!(p(&["status", "extra"]).is_err());
        assert!(p(&["daemon", "stop", "now"]).is_err());
    }
}
