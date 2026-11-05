//! Command-line entry point: argument parsing, help, and wiring the real clock and runner.

use crate::classify::classify;
use crate::clock::RealClock;
use crate::config::{config_path, Config};
use crate::runner::{die_by_signal, stdin_is_tty, RealRunner};
use crate::state::{self, Paths};
use crate::status;
use crate::wrapper::{answers_quit, Outcome, Wrapper, EXIT_NO_GH, EXIT_REFUSED, EXIT_USAGE};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Exit status for a configuration error (bad config file, bad environment value, bad path).
pub const EXIT_CONFIG: i32 = 78;
/// Deepest allowed nesting of gh-paced inside gh (an extension or alias that calls gh again).
pub const MAX_DEPTH: u32 = 8;
/// Real gh locations tried, in order, when neither `--real-gh` nor `GH_PACED_REAL_GH` is set.
/// `PATH` is never searched: on a paced host `gh` on `PATH` is the wrapper itself.
pub const DEFAULT_REAL_GH: [&str; 2] = ["/usr/bin/gh", "/usr/local/bin/gh"];

const HELP: &str = "\
gh-paced: client-side rate limiting for the GitHub CLI

gh-paced sits between an account shim and the real gh. Every call is classified
(READ, SEARCH, WRITE, GIT_CREDENTIAL or LOCAL), charged against per-host,
per-account budgets shared by every process on the host through a locked state
file, and delayed with a loud stderr warning when a budget is exhausted. It
also watches GitHub's own account-wide numbers (GET /rate_limit), backs off
for at least 15 minutes after any rate-limit, abuse or HTTP 403 response, and
refuses oversized or base64-laden write bodies. It expands gh's ordinary
aliases itself and runs gh with the expansion, so gh never sees an ordinary
alias's name; a shell alias (`!...`) is passed to gh by name and charged one
WRITE, and what its shell command sends is paced only if the gh it runs is
gh-paced. gh still reads its config.yml again when it starts, and a config.yml
rewritten during the call is not guarded against. When config.yml cannot be
read, every command is refused (exit 78). It checks bodies given as arguments,
files or stdin, and text typed in the editor gh opens for a write (gh's
editor is pointed at `gh-paced --edit-guard`, which runs your editor and then
checks the file).
Body files are copied privately so gh sends exactly what was checked; bodies
gh would compose without an editor (`pr create` prompts, --fill, templates)
are refused.

gh's output passes through unchanged, including anything gh prints that
contains a credential (`gh auth token`, `gh auth status --show-token`, the git
credential helper). stderr, and stdout for `gh api --include`, are scanned for
GitHub's refusals as they stream. The audit log records a short summary of
each call (class, cost, command, and arguments with inline bodies replaced by
their sizes and header values redacted); it holds no request bodies and no
environment. Output that a process started by gh writes after gh exits is
delivered by a separate `gh-paced --drain` process, so gh-paced exits when gh
does. `--edit-guard` and `--drain` are run by gh and gh-paced themselves.

USAGE
  gh-paced [--account NAME] [--real-gh PATH] -- <gh arguments...>
  gh-paced status [--account NAME | --all] [--json]
  gh-paced classify [--json] -- <gh arguments...>
  gh-paced quickstart | userguide | help | --help | -h | --version

PASS-THROUGH OPTIONS (before the required `--`)
  --account NAME   GitHub login whose budgets apply (letters, digits, hyphens;
                   at most 39). Default: $GH_PACED_ACCOUNT. Required.
  --real-gh PATH   Absolute path of the real gh. Default: $GH_PACED_REAL_GH,
                   else /usr/bin/gh, else /usr/local/bin/gh. PATH is never
                   searched, and a path that resolves to gh-paced is refused.

SUBCOMMANDS
  status           Show budgets, in-flight writes, cooldown, the cached GitHub
                   snapshot and recent audit records. Reads local state only;
                   never contacts GitHub. `gh-paced status --help` for options.
  classify         Print how a gh command line would be classified and charged.
                   Touches no state and runs nothing.
  quickstart       One-screen introduction.
  userguide        Full reference: classes, budgets with GitHub citations,
                   configuration, messages, multi-host arithmetic.

DEFAULT BUDGETS (per host, per account; four hosts assumed)
  READ             20/min, burst 10, 500/hour
  SEARCH           5/min, burst 2, 150/hour
  WRITE            1 per 30 s, burst 1, 30/hour, 1 in flight
  GIT_CREDENTIAL   1 per 10 s, burst 1, 120/hour; waits at most 30 s, and is
                   refused at once (exit 75) during a cooldown, so git fails
                   fast instead of holding its caller's locks
  LOCAL            unpaced, unaudited (help, completion, config, alias, ...)
  gh's aliases are expanded first and paced as the command they name.
  Unknown commands (extensions) are WRITE, even with --help.
  `gh api --paginate` costs 10 tokens, for writes too. A call costing more
  than its class's burst (for example a SEARCH --paginate, or a --limit that
  needs more pages than the burst) is refused at once (exit 75): one gh call
  makes its requests back to back. A cost above the hourly cap is refused too.
  Watch loops (`gh pr checks --watch`, `gh run watch`) are refused (exit 64)
  unless GH_PACED_ALLOW_WATCH=1: gh-paced cannot count a poll's requests. With
  it they cost 20, an estimate, and are stopped (exit 75) once they outlast the
  polls that estimate allows; a large or failing run can make more requests.

ENVIRONMENT
  GH_PACED_ACCOUNT           default for --account
  GH_PACED_REAL_GH           default for --real-gh
  GH_PACED_MAX_WAIT          longest total sleep before refusing, seconds
                             (default 900); a longer wait exits 75
  GH_PACED_GIT_MAX_WAIT      longest total wait for a GIT_CREDENTIAL call,
                             seconds (default 30; the smaller of this and
                             GH_PACED_MAX_WAIT applies), including its waits
                             for the state lock; a longer wait exits 75 (70
                             when the lock was not free in time). During a
                             cooldown a GIT_CREDENTIAL call is refused without
                             waiting at all, and one that becomes admissible
                             only after its bound is refused (75). A
                             GIT_CREDENTIAL call that fails before gh starts
                             (127 included) prints `quit=1` on stdout, which
                             makes git stop at once instead of prompting
  GH_PACED_LOCK_WAIT         longest wait for the state lock, seconds
                             (default 30, 0.1 to 3600); then exit 70
  GH_PACED_{READ,SEARCH,WRITE,GIT}_{PER_MINUTE,BURST,PER_HOUR}
                             tighten a budget (a looser value is ignored with
                             a warning)
  GH_PACED_PAGINATE_COST     raise the --paginate cost (lower is ignored)
  GH_PACED_DISPLAY_TZ        US-Eastern (default) or UTC for printed times
  GH_PACED_ALLOW_LARGE_BODY  1 = skip the write content guard for this call
  GH_PACED_ALLOW_WATCH       1 = run watch loops at their estimated charge
  GH_PACED_ALLOW_FAST_WATCH  1 = allow watch intervals under 30 s
  GH_PACED_EDITOR            set by gh-paced for gh's editor guard: the
                             editor you would have had (GH_EDITOR, gh's
                             `editor` setting, GIT_EDITOR, VISUAL, EDITOR,
                             nano); refused editor text is kept in the state
                             directory as ACCOUNT.refused-edit.*.md
  GH_PACED_STATE_DIR         state directory (default
                             $XDG_STATE_HOME/gh-paced or ~/.local/state/gh-paced)
  GH_PACED_CONFIG            config file (default
                             $XDG_CONFIG_HOME/gh-paced/config.json or
                             ~/.config/gh-paced/config.json)

EXIT STATUS
  gh's own status (or gh-paced dies by the same signal), except:
  75  refused: the wait would exceed GH_PACED_MAX_WAIT (GH_PACED_GIT_MAX_WAIT
      for GIT_CREDENTIAL), a GIT_CREDENTIAL call arrived during a cooldown,
      the cost is above the class's burst or can never fit the hourly cap, a
      watch outlasted the polls its cost allows, or nesting too deep
  65  refused by the write content guard (body > 8 KiB, base64 run > 1000, a
      body gh composes itself, or a body file that cannot be copied). Text
      refused in the editor makes the editor fail, so gh itself exits non-zero
      (gh reports its own status, usually 1) and sends nothing.
  64  usage error, or a refused command shape (a watch without
      GH_PACED_ALLOW_WATCH=1, a watch interval under 30 s, or one that is not
      a plain positive whole number of seconds)
  70  internal error: pacing state cannot be read or written, or its lock was
      not obtained within GH_PACED_LOCK_WAIT (within what is left of
      GH_PACED_GIT_MAX_WAIT for GIT_CREDENTIAL)
  78  configuration error
  127 the real gh cannot be found or run

EXAMPLES
  gh-paced --account octocat -- pr view 12 --json state
  GH_PACED_MAX_WAIT=60 gh-paced --account octocat -- issue comment 7 --body-file note.md
  gh-paced status --account octocat
  gh-paced classify -- api -X POST repos/o/r/issues/1/comments -f body=hi
";

const STATUS_HELP: &str = "\
gh-paced status: show one account's pacing state (local files only)

USAGE
  gh-paced status [--account NAME | --all] [--json]

OPTIONS
  --account NAME  Account to show. Default: $GH_PACED_ACCOUNT.
  --all           Show every account with a state file in the state directory.
  --json          Print JSON instead of text.

Shows, per class: tokens available and burst, refill rate, cost used in the
last hour against the hourly cap, and seconds until the next one-token call
fits. Also in-flight writes (PID, age, liveness), any active cooldown with its
reason, the cached GET /rate_limit numbers and their age, and the last 5 audit
records. Takes no lock and creates no files; never contacts GitHub, never
refreshes, never writes state.

EXAMPLES
  gh-paced status --account octocat
  gh-paced status --all --json
";

const CLASSIFY_HELP: &str = "\
gh-paced classify: show how a gh command line would be paced

USAGE
  gh-paced classify [--json] -- <gh arguments...>

OPTIONS
  --json  Print JSON (class, cost, command, reason, warnings, refusal).

Runs nothing and touches no state or network. Classification depends only on
the arguments, the configuration (for --paginate and watch costs) and gh's own
aliases (read from gh's config.yml), which are expanded first, as a real run
expands them; the expansion is shown on an `alias:` line (an `alias` key with
--json). REFUSED shows only the refusals decided from the command line, the
configuration and gh's aliases: a watch without GH_PACED_ALLOW_WATCH=1, an
unreadable or too-short --interval on a watch, an alias gh-paced cannot
resolve, and every command when gh's config.yml cannot be read (a real run
refuses that with exit 78). A real run can still refuse what classify accepts:
a body the content guard rejects (exit 65), and a cost larger than the bucket
or a wait beyond GH_PACED_MAX_WAIT (exit 75).

EXAMPLES
  gh-paced classify -- pr comment 5 --body hi
  gh-paced classify --json -- api graphql -f query='query { viewer { login } }'
";

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The variable as the operating system gives it, including a value that is not UTF-8 (which
/// [`env_var`] reports as unset).
fn env_os(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name)
}

fn loud_error(text: &str) {
    eprintln!("GH-PACED ERROR {text}");
}

fn to_strings(args: impl Iterator<Item = OsString>) -> Result<Vec<String>, String> {
    args.map(|a| {
        a.into_string()
            .map_err(|a| format!("argument {a:?} is not valid UTF-8; gh-paced cannot classify it"))
    })
    .collect()
}

fn load_config() -> Result<Config, i32> {
    let path = config_path(&env_var);
    let (path, explicit) = match &path {
        Some((p, e)) => (Some(p.as_path()), *e),
        None => (None, false),
    };
    match Config::load(path, explicit, &env_var) {
        Ok((cfg, warnings)) => {
            for w in warnings {
                eprintln!("GH-PACED WARNING {w}");
            }
            Ok(cfg)
        }
        Err(e) => {
            loud_error(&format!("configuration: {e}"));
            Err(EXIT_CONFIG)
        }
    }
}

fn account_from(opt: Option<String>) -> Result<String, i32> {
    let Some(account) = opt.or_else(|| env_var("GH_PACED_ACCOUNT").filter(|a| !a.is_empty()))
    else {
        loud_error("no account: pass --account NAME or set GH_PACED_ACCOUNT (see gh-paced --help)");
        return Err(EXIT_USAGE);
    };
    if !state::valid_account(&account) {
        loud_error(&format!(
            "account {account:?} is not a valid GitHub login (letters, digits, hyphens; at most 39)"
        ));
        return Err(EXIT_USAGE);
    }
    Ok(account)
}

/// Resolve the real gh from an explicit option, the environment, then the fixed locations.
pub fn resolve_real_gh(
    opt: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
    self_exe: Option<&Path>,
) -> Result<PathBuf, (i32, String)> {
    let chosen: Option<(PathBuf, &str)> = match opt {
        Some(p) => Some((PathBuf::from(p), "--real-gh")),
        None => env("GH_PACED_REAL_GH")
            .filter(|p| !p.is_empty())
            .map(|p| (PathBuf::from(p), "GH_PACED_REAL_GH")),
    };
    let path = match chosen {
        Some((p, source)) => {
            if !p.is_absolute() {
                return Err((
                    EXIT_CONFIG,
                    format!("{source} must be an absolute path, got {}", p.display()),
                ));
            }
            if !p.is_file() {
                return Err((
                    EXIT_NO_GH,
                    format!("{source} {} does not exist or is not a file", p.display()),
                ));
            }
            p
        }
        None => match DEFAULT_REAL_GH
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
        {
            Some(p) => p,
            None => {
                return Err((
                    EXIT_NO_GH,
                    format!(
                        "no real gh found at {}; pass --real-gh PATH",
                        DEFAULT_REAL_GH.join(" or ")
                    ),
                ))
            }
        },
    };
    if let (Ok(real), Some(me)) = (
        path.canonicalize(),
        self_exe.and_then(|p| p.canonicalize().ok()),
    ) {
        if real == me {
            return Err((
                EXIT_CONFIG,
                format!(
                    "real gh {} is gh-paced itself; refusing to recurse",
                    path.display()
                ),
            ));
        }
    }
    Ok(path)
}

fn short_host() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into a valid buffer.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "unknown".to_string();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let full = String::from_utf8_lossy(&buf[..end]).to_string();
    full.split('.').next().unwrap_or("unknown").to_string()
}

fn read_stdin_capped(limit: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::io::stdin()
        .lock()
        .take(u64::try_from(limit).unwrap_or(u64::MAX))
        .read_to_end(&mut buf)?;
    Ok(buf)
}

fn run_paced(account: Option<String>, real_gh: Option<String>, gh_args: Vec<String>) -> i32 {
    // Recognised before anything can refuse it. Only a command line naming `git-credential` is
    // classified this early, so no other command is inspected twice.
    let mut credential = gh_args.iter().any(|a| a == "git-credential")
        && classify(&gh_args, &Config::default()).class == crate::classify::Class::GitCredential;
    let mut gh_started = false;
    let outcome = paced_outcome(account, real_gh, &gh_args, &mut credential, &mut gh_started);
    if answers_quit(credential, gh_started, outcome) {
        // git then stops at once instead of trying another helper or prompting on a terminal
        // while it holds its caller's locks.
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(b"quit=1\n").and_then(|()| out.flush());
    }
    match outcome {
        Outcome::Exit(code) => code,
        Outcome::Signal(sig) => die_by_signal(sig),
    }
}

/// Run one paced command line. Sets `credential` when the wrapper classifies it as
/// GIT_CREDENTIAL (after gh alias expansion), and `gh_started` once gh was started.
fn paced_outcome(
    account: Option<String>,
    real_gh: Option<String>,
    gh_args: &[String],
    credential: &mut bool,
    gh_started: &mut bool,
) -> Outcome {
    let account = match account_from(account) {
        Ok(a) => a,
        Err(code) => return Outcome::Exit(code),
    };
    let depth: u32 = env_var("GH_PACED_DEPTH")
        .and_then(|d| d.trim().parse().ok())
        .unwrap_or(0);
    if depth >= MAX_DEPTH {
        eprintln!(
            "GH-PACED REFUSED [{account}] gh-paced is nested {depth} deep (limit {MAX_DEPTH}); \
             an alias or extension is probably calling gh in a loop (exit {EXIT_REFUSED})"
        );
        return Outcome::Exit(EXIT_REFUSED);
    }
    let self_exe = std::env::current_exe().ok();
    let real_gh = match resolve_real_gh(real_gh.as_deref(), &env_var, self_exe.as_deref()) {
        Ok(p) => p,
        Err((code, e)) => {
            loud_error(&e);
            return Outcome::Exit(code);
        }
    };
    let cfg = match load_config() {
        Ok(c) => c,
        Err(code) => return Outcome::Exit(code),
    };
    let dir = match state::state_dir(&env_var) {
        Ok(d) => d,
        Err(e) => {
            loud_error(&format!("state directory: {e}"));
            return Outcome::Exit(EXIT_CONFIG);
        }
    };
    let chain: Vec<String> = env_var("GH_PACED_INFLIGHT_CHAIN")
        .map(|c| {
            c.split(',')
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let clock = RealClock;
    let runner = RealRunner;
    let pid = std::process::id();
    let now = crate::clock::Clock::now(&clock);
    let paths = Paths::new(dir, &account);
    let probe = paths.clone();
    let alive = move |h: &state::Holder| state::holder_alive_in(&probe, h);
    let mut w = Wrapper {
        clock: &clock,
        runner: &runner,
        cfg,
        paths,
        host: short_host(),
        real_gh,
        echo: true,
        messages: Vec::new(),
        printed: 0,
        stdin_is_tty: stdin_is_tty(),
        stdin_reader: Box::new(read_stdin_capped),
        chain,
        depth,
        alive: &alive,
        inherited_leases: state::inherited_locked_file_ids(),
        pid,
        start_ticks: state::process_start_ticks(pid).unwrap_or(0),
        nonce: state::new_nonce(now),
        gh_aliases: crate::alias::GhAliases::load_checked(&env_var, &env_os),
        alias_note: None,
        body_bytes_used: 0,
        editor_guard_env: crate::editor::guard_env(&env_var, self_exe.as_deref(), &account),
        self_exe,
        stderr_deadline: None,
        gh_started: false,
        credential_started: None,
    };
    let outcome = w.run(gh_args);
    *credential |= w.credential_started.is_some();
    *gh_started = w.gh_started;
    outcome
}

/// `gh-paced --drain ACCOUNT COMMAND STREAM`, started by gh-paced itself (see
/// [`crate::runner::drain`]). Pushback in what it delivers is recorded as a cooldown for
/// ACCOUNT and COMMAND, unless COMMAND is empty or the configuration or state directory cannot
/// be read; the bytes are delivered either way. Configuration warnings were already printed by
/// the gh-paced that started it, so they are not repeated into the stream.
fn run_drain(args: &[String]) -> i32 {
    use crate::clock::Clock;
    let (Some(account), Some(command), Some(which)) = (
        args.first(),
        args.get(1),
        args.get(2)
            .and_then(|s| crate::runner::Stream::from_name(s)),
    ) else {
        loud_error(&format!(
            "{} is started by gh-paced itself: gh-paced {} ACCOUNT COMMAND err|out",
            crate::wrapper::DRAIN_FLAG,
            crate::wrapper::DRAIN_FLAG
        ));
        return EXIT_USAGE;
    };
    let mut hook = None;
    if !command.is_empty() && state::valid_account(account) {
        let path = config_path(&env_var);
        let (path, explicit) = match &path {
            Some((p, e)) => (Some(p.as_path()), *e),
            None => (None, false),
        };
        if let (Ok((cfg, _)), Ok(dir)) = (
            Config::load(path, explicit, &env_var),
            state::state_dir(&env_var),
        ) {
            let paths = Paths::new(dir, account);
            let (account, command) = (account.clone(), command.clone());
            hook = Some(crate::runner::PushbackHook(std::sync::Arc::new(
                move |s: &crate::pushback::Scanner| {
                    let Some(pb) = s.verdict(&cfg) else {
                        return;
                    };
                    let recorded = crate::wrapper::publish_cooldown(
                        &paths,
                        cfg.lock_wait_secs,
                        RealClock.now(),
                        &pb,
                        &command,
                    );
                    let what = match recorded {
                        Ok(_) if pb.has_no_end() => {
                            "cooldown with no end time recorded".to_string()
                        }
                        Ok(_) => format!("cooldown of {:.0} s recorded", pb.cooldown_secs),
                        Err(e) => format!("the cooldown could not be recorded: {e}"),
                    };
                    let line = format!(
                        "GH-PACED PUSHBACK [{account}] {} in output written after gh exited \
                         (`{command}`): {what}. Never switch accounts to get around it.\n",
                        pb.reason
                    );
                    crate::runner::write_fd_all(2, line.as_bytes());
                },
            )));
        }
    }
    crate::runner::drain(which, hook.as_ref())
}

fn run_status(args: &[String]) -> i32 {
    let mut account = None;
    let mut all = false;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print!("{STATUS_HELP}");
                return 0;
            }
            "--all" => all = true,
            "--json" => json = true,
            "--account" => {
                i += 1;
                let Some(a) = args.get(i) else {
                    loud_error("--account needs a value");
                    return EXIT_USAGE;
                };
                account = Some(a.clone());
            }
            other => {
                if let Some(a) = other.strip_prefix("--account=") {
                    account = Some(a.to_string());
                } else {
                    loud_error(&format!(
                        "status: unknown argument {other:?} (see gh-paced status --help)"
                    ));
                    return EXIT_USAGE;
                }
            }
        }
        i += 1;
    }
    let cfg = match load_config() {
        Ok(c) => c,
        Err(code) => return code,
    };
    let dir = match state::state_dir(&env_var) {
        Ok(d) => d,
        Err(e) => {
            loud_error(&format!("state directory: {e}"));
            return EXIT_CONFIG;
        }
    };
    let accounts = if all {
        if account.is_some() {
            loud_error("status: --all and --account are exclusive");
            return EXIT_USAGE;
        }
        status::accounts(&dir)
    } else {
        match account_from(account) {
            Ok(a) => vec![a],
            Err(code) => return code,
        }
    };
    let now = crate::clock::Clock::now(&RealClock);
    let mut reports = Vec::new();
    let mut rc = 0;
    for a in &accounts {
        match status::report(&Paths::new(dir.clone(), a), &cfg, now) {
            Ok(r) => reports.push(r),
            Err(e) => {
                loud_error(&format!("status for {a}: {e}"));
                rc = EXIT_CONFIG;
            }
        }
    }
    if json {
        let v = if all {
            serde_json::Value::Array(reports)
        } else {
            reports
                .into_iter()
                .next()
                .unwrap_or(serde_json::Value::Null)
        };
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else if reports.is_empty() && all {
        println!("gh-paced: no account state in {}", dir.display());
    } else {
        for r in &reports {
            print!("{}", status::render(r));
        }
    }
    rc
}

fn run_classify(args: &[String]) -> i32 {
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print!("{CLASSIFY_HELP}");
                return 0;
            }
            "--json" => json = true,
            "--" => break,
            other => {
                loud_error(&format!(
                    "classify: unknown argument {other:?}; put gh arguments after `--`"
                ));
                return EXIT_USAGE;
            }
        }
        i += 1;
    }
    if i >= args.len() {
        loud_error("classify: missing `--` before the gh arguments");
        return EXIT_USAGE;
    }
    let cfg = match load_config() {
        Ok(c) => c,
        Err(code) => return code,
    };
    let mut gh_args = args[i + 1..].to_vec();
    let mut alias_note = None;
    let mut alias_refusal = None;
    match crate::alias::GhAliases::load_checked(&env_var, &env_os).resolve(&gh_args) {
        crate::alias::Resolution::NotAlias => {}
        crate::alias::Resolution::Expanded { argv, chain } => {
            alias_note = Some(format!(
                "`{}` -> `{}`",
                chain.join("` -> `"),
                argv.join(" ")
            ));
            gh_args = argv;
        }
        crate::alias::Resolution::Shell { argv, chain } => {
            alias_note = Some(format!(
                "shell alias `{}`, which gh runs with `sh -c`",
                chain.join("` -> `")
            ));
            gh_args = argv;
        }
        crate::alias::Resolution::Refused { reason, .. } => alias_refusal = Some(reason),
    }
    let mut c = classify(&gh_args, &cfg);
    if let Some(r) = alias_refusal {
        c.refusal = Some(r);
    }
    if json {
        let v = serde_json::json!({
            "class": c.class.name(),
            "cost": c.cost,
            "command": c.command,
            "reason": c.reason,
            "warnings": c.warnings,
            "refusal": c.refusal,
            "alias": alias_note,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else {
        println!("class:   {}", c.class.name());
        println!("cost:    {}", c.cost);
        println!("command: {}", c.command);
        println!("reason:  {}", c.reason);
        if let Some(a) = &alias_note {
            println!("alias:   {a}");
        }
        for w in &c.warnings {
            println!("warning: {w}");
        }
        if let Some(r) = &c.refusal {
            println!("REFUSED: {r}");
        }
    }
    0
}

/// Run the command line (without the program name) and return the exit status.
pub fn main(args: impl Iterator<Item = OsString>) -> i32 {
    let args = match to_strings(args) {
        Ok(a) => a,
        Err(e) => {
            loud_error(&e);
            return EXIT_USAGE;
        }
    };
    match args.first().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => {
            print!("{HELP}");
            return if args.is_empty() { EXIT_USAGE } else { 0 };
        }
        Some("--version") => {
            println!("gh-paced {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        Some("quickstart") => {
            print!("{}", crate::QUICKSTART);
            return 0;
        }
        Some("userguide") => {
            print!("{}", crate::USER_GUIDE);
            return 0;
        }
        Some("status") => return run_status(&args[1..]),
        Some(crate::editor::EDIT_GUARD_FLAG) => {
            let cfg = match load_config() {
                Ok(c) => c,
                Err(code) => return code,
            };
            return crate::editor::run_guard(&args[1..], &env_var, &cfg);
        }
        Some(crate::wrapper::DRAIN_FLAG) => return run_drain(&args[1..]),
        Some("classify") => return run_classify(&args[1..]),
        _ => {}
    }
    let mut account = None;
    let mut real_gh = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--" => {
                return run_paced(account, real_gh, args[i + 1..].to_vec());
            }
            "--account" | "--real-gh" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    loud_error(&format!("{a} needs a value"));
                    return EXIT_USAGE;
                };
                if a == "--account" {
                    account = Some(v.clone());
                } else {
                    real_gh = Some(v.clone());
                }
            }
            _ => {
                if let Some(v) = a.strip_prefix("--account=") {
                    account = Some(v.to_string());
                } else if let Some(v) = a.strip_prefix("--real-gh=") {
                    real_gh = Some(v.to_string());
                } else {
                    loud_error(&format!(
                        "unknown argument {a:?}; gh arguments go after `--` (gh-paced --account NAME -- <gh args>)"
                    ));
                    return EXIT_USAGE;
                }
            }
        }
        i += 1;
    }
    loud_error("missing `--` before the gh arguments (gh-paced --account NAME -- <gh args>)");
    EXIT_USAGE
}
