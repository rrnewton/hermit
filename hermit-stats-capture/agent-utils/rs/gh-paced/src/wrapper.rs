//! One paced gh invocation: classify, copy the files it sends, guard, wait for admission
//! (refreshing the account snapshot when due), run gh, and record the outcome.
//!
//! Every decision is made while holding the account lock and against a time read after the lock
//! was taken, so a process that waited for the lock cannot charge a slot in the past.

use crate::alias::Resolution;
use crate::audit::{self, Record};
use crate::budget::{halved, Bucket};
use crate::classify::{classify, Class, Classification};
use crate::cli::EXIT_CONFIG;
use crate::clock::{Clock, RealClock};
use crate::config::{ClassLimits, Config};
use crate::guard::{self, Verdict};
use crate::pushback::{has_no_end, Pushback, Scanner};
use crate::ratelimit;
use crate::runner::{
    write_stderr_bounded, DrainCommand, Exit, Invocation, PushbackHook, Ran, Runner,
    MAX_CAPTURE_BYTES,
};
use crate::snapshot;
use crate::state::{self, Cooldown, Holder, Lease, Paths, State};
use crate::timefmt::human;
use std::io::Write;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Exit status when a budget, cooldown or recursion limit refuses the call.
pub const EXIT_REFUSED: i32 = 75;
/// Exit status when the content guard refuses a write body.
pub const EXIT_CONTENT: i32 = 65;
/// Exit status for a usage error or a refused command shape (fast watch).
pub const EXIT_USAGE: i32 = 64;

/// The argument that runs gh-paced as a drainer: `gh-paced --drain ACCOUNT COMMAND STREAM`
/// (see [`crate::runner::drain`]). COMMAND is the call's command for pushback cooldowns; empty
/// for a LOCAL call, whose output records no pushback.
pub const DRAIN_FLAG: &str = "--drain";
/// Exit status for an internal error (state cannot be locked, read or written).
pub const EXIT_INTERNAL: i32 = 70;
/// Exit status when the real gh cannot be run.
pub const EXIT_NO_GH: i32 = 127;

/// How often a WRITE waiting for another in-flight WRITE re-checks, seconds. Every re-check
/// prints a warning and writes a `throttle` audit record.
pub const IN_FLIGHT_POLL_SECS: f64 = 5.0;
/// After a signal arrives while gh's output is still being delivered, how long gh-paced's own
/// messages may take to reach stderr before the rest are dropped (they stay in the audit log).
pub const LATE_SIGNAL_STDERR_SECS: f64 = 2.0;
/// How often a call waiting for another process's rate-limit refresh re-checks, seconds.
pub const REFRESH_POLL_SECS: f64 = 1.0;
/// The `command` recorded on the pause set by [`recovery_state`].
pub const RECOVERY_COMMAND: &str = "(state recovery)";

/// How the invocation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Exit with this status.
    Exit(i32),
    /// gh was killed by this signal; die by it too.
    Signal(i32),
}

/// Reads up to `n` bytes of stdin (until EOF or `n`).
pub type StdinReader<'a> = Box<dyn FnMut(usize) -> std::io::Result<Vec<u8>> + 'a>;

/// Everything one invocation needs. Tests build it with a fake clock and runner.
pub struct Wrapper<'a> {
    /// Time source.
    pub clock: &'a dyn Clock,
    /// Process runner.
    pub runner: &'a dyn Runner,
    /// Effective configuration.
    pub cfg: Config,
    /// State file paths for the account.
    pub paths: Paths,
    /// Short host name for messages and audit.
    pub host: String,
    /// Absolute path of the real gh.
    pub real_gh: PathBuf,
    /// Print messages to stderr as well as collecting them.
    pub echo: bool,
    /// Every message printed (for tests and for the caller).
    pub messages: Vec<String>,
    /// Construct with 0. How many of [`Wrapper::messages`] have been written to stderr.
    /// Messages are queued while the account lock may be held and written only at points where
    /// it is not (before a sleep, before gh starts, at the end), so a consumer that has stopped
    /// reading stderr stalls this invocation alone and never the account's other gh calls.
    pub printed: usize,
    /// gh-paced's stdin is a terminal.
    pub stdin_is_tty: bool,
    /// Reads stdin when the content guard must inspect it.
    pub stdin_reader: StdinReader<'a>,
    /// Nonces of enclosing gh-paced invocations (`GH_PACED_INFLIGHT_CHAIN`).
    pub chain: Vec<String>,
    /// Nesting depth (`GH_PACED_DEPTH`).
    pub depth: u32,
    /// Liveness test for in-flight holders and refresh claims (production:
    /// `state::holder_alive_in`, the lease lock when there is one).
    pub alive: &'a dyn Fn(&Holder) -> bool,
    /// Device and inode of every regular file this process holds through a descriptor that
    /// itself holds an exclusive `flock` (`state::inherited_locked_file_ids`). An in-flight WRITE
    /// is treated as this invocation's own ancestor, and not waited for, only when its nonce is
    /// in [`Wrapper::chain`] AND its lease file is in this list. Only a holder of that WRITE's
    /// own locked open file (its gh, or a process gh started) qualifies; opening the lease file
    /// again does not.
    pub inherited_leases: Vec<(u64, u64)>,
    /// This process's ID.
    pub pid: u32,
    /// This process's start time in clock ticks.
    pub start_ticks: u64,
    /// This invocation's random identifier.
    pub nonce: String,
    /// gh's own command aliases (`alias::GhAliases::load`). A command line that invokes one is
    /// expanded before anything else looks at it, and gh is given the expansion.
    pub gh_aliases: crate::alias::GhAliases,
    /// The gh alias this invocation expanded, named on every refusal banner (set by
    /// [`Wrapper::run`]; `None` to start).
    pub alias_note: Option<String>,
    /// Variables that make gh open its editor through `gh-paced --edit-guard`
    /// (`editor::guard_env`), added to the environment of every WRITE call while the content
    /// guard is on, so text gh composes in an editor is checked too. Empty: no editor guard.
    pub editor_guard_env: Vec<(String, String)>,
    /// Body bytes the content guard admitted from this call's arguments and stdin (set by the
    /// content guard; 0 to start), passed to the editor guard as
    /// [`crate::editor::PRIOR_BYTES_VAR`] so both share one size allowance.
    pub body_bytes_used: u64,
    /// gh-paced's own executable, run as `gh-paced --drain` to keep delivering a stream that a
    /// process gh started still holds after gh exits (see the runner module). `None`: such a
    /// stream is closed after the runner's cutoffs, and a warning says later output was lost.
    pub self_exe: Option<std::path::PathBuf>,
    /// Construct with `None`. Set once a signal arrives while gh's output is still being
    /// delivered: messages are then written to stderr only until this instant, each write given
    /// up when it would wait past it, so a consumer that has stopped reading cannot keep
    /// gh-paced from dying by the signal.
    pub stderr_deadline: Option<Instant>,
    /// Construct with `false`. Set just before gh is started, and cleared again when the runner
    /// reports that gh's program could not be executed, so the caller knows whether gh could
    /// have written to stdout: a GIT_CREDENTIAL call that ends without starting gh is
    /// answered with `quit=1` (see `cli::run_paced`).
    pub gh_started: bool,
    /// Construct with `None`. The monotonic clock reading ([`Clock::monotonic`], which setting
    /// the system time does not move) at which a GIT_CREDENTIAL call
    /// (`auth git-credential get`) was first recognised, before gh's aliases are resolved, or
    /// after that when only an alias's expansion is one. Its wait bound (`Config::max_wait`)
    /// counts from here, and covers every wait: for its budget and for the state lock, before and
    /// after gh runs. Recording pushback is the exception: it is cleared once gh's output shows
    /// pushback, so that cooldown is recorded within the whole GH_PACED_LOCK_WAIT.
    pub credential_started: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitKind {
    Cooldown,
    Block,
    InFlight,
    Refresh,
    Bucket,
    Hour,
    RefreshBudget,
}

impl WaitKind {
    /// A polled wait re-checks every few seconds instead of sleeping until a known time.
    fn polled(self) -> bool {
        matches!(self, WaitKind::InFlight | WaitKind::Refresh)
    }
}

/// What the refresh step decided for this admission pass.
enum RefreshStep {
    /// Nothing to do: proceed with the snapshot there is (possibly none).
    Skip,
    /// This process claimed the refresh and charged its READ token; run it outside the lock.
    Run,
    /// There is no snapshot and another live process is fetching one: wait for it.
    WaitForOther,
    /// A refresh is due but the READ budget has no token for it yet: wait this long (subject
    /// to `GH_PACED_MAX_WAIT`) rather than run without current account feedback.
    WaitForRead(f64),
}

/// Whether a call answers `quit=1` on stdout (the git credential protocol's "stop now"): a
/// GIT_CREDENTIAL call that ended without starting gh, refused or unable to run. git ignores a
/// helper's exit status; without `quit=1` it would try another helper or prompt on a terminal.
/// Never once gh started, since gh may already have answered.
pub fn answers_quit(credential: bool, gh_started: bool, outcome: Outcome) -> bool {
    credential && !gh_started && outcome != Outcome::Exit(0)
}

/// A call that passed admission.
struct Admitted {
    waited: f64,
    lease: Option<Lease>,
}

/// Record the cooldown `pb` calls for in the account's cooldown record, unless a longer one is
/// already recorded, as soon as gh's output shows the pushback. Prints nothing: it runs on a
/// reader thread while gh's output streams, and [`Wrapper::finish`] prints the banner and
/// records the same cooldown in the state file afterwards. True when the record was written.
pub fn publish_cooldown(
    paths: &Paths,
    lock_wait_secs: f64,
    now: f64,
    pb: &Pushback,
    command: &str,
) -> Result<bool, String> {
    let _guard = state::lock_within(paths, lock_wait_secs)?;
    let cd = Cooldown {
        until: pb.until(now),
        set_at: now,
        reason: pb.reason.clone(),
        command: command.to_string(),
    };
    state::extend_cooldown(paths, &cd)
}

/// State used in place of an unusable state file: every class blocked for the next hour
/// whatever its limits are by then (the lost history may have held a full hour of calls) and a
/// pause of `cooldown_secs`.
pub fn recovery_state(cfg: &Config, now: f64) -> State {
    let mut st = State::new();
    for class in Class::PACED {
        st.buckets
            .insert(class.name().to_string(), Bucket::saturated(now));
    }
    st.cooldown = Some(Cooldown {
        until: now + cfg.cooldown_secs,
        set_at: now,
        reason: "the pacing state file was unusable and was quarantined".to_string(),
        command: RECOVERY_COMMAND.to_string(),
    });
    st
}

#[derive(Debug, Clone)]
struct Wait {
    secs: f64,
    kind: WaitKind,
    text: String,
}

fn fmt_num(x: f64) -> String {
    if (x - x.round()).abs() < 1e-9 {
        format!("{}", x.round() as i64)
    } else {
        format!("{x:.1}")
    }
}

/// Human description of a refill rate: `1 per 30 s` for slow rates, `20/min` otherwise.
pub fn describe_rate(l: ClassLimits) -> String {
    if l.per_minute <= 0.0 {
        return "0/min".to_string();
    }
    let interval = 60.0 / l.per_minute;
    if interval >= 10.0 - 1e-9 {
        format!("1 per {} s", fmt_num(interval))
    } else {
        format!("{}/min", fmt_num(l.per_minute))
    }
}

fn paced_api(class: Class) -> bool {
    matches!(class, Class::Read | Class::Search | Class::Write)
}

impl Wrapper<'_> {
    /// Queue one message. [`Wrapper::flush`] writes it to stderr.
    fn emit(&mut self, line: String) {
        self.messages.push(line);
    }

    /// Write the queued messages to stderr. Call only where the account lock is not held.
    fn flush(&mut self) {
        if self.printed >= self.messages.len() {
            return;
        }
        if self.echo {
            let mut text = String::new();
            for line in &self.messages[self.printed..] {
                text.push_str(line);
                text.push('\n');
            }
            match self.stderr_deadline {
                None => {
                    let _ = std::io::stderr().write_all(text.as_bytes());
                }
                Some(deadline) => {
                    write_stderr_bounded(text.into_bytes(), deadline);
                }
            }
        }
        self.printed = self.messages.len();
    }

    fn loud(&mut self, kind: &str, text: &str) {
        let line = format!("GH-PACED {kind} [{}] {text}", self.paths.account);
        self.emit(line);
    }

    fn banner(&mut self, kind: &str, lines: &[String]) {
        let rule = format!("GH-PACED {}", "*".repeat(68));
        self.emit(rule.clone());
        for l in lines {
            self.loud(kind, l);
        }
        if kind == "REFUSED" {
            if let Some(note) = self.alias_note.clone() {
                self.loud(kind, &note);
            }
        }
        self.emit(rule);
    }

    fn when(&self, at: f64) -> String {
        human(at, self.clock.now(), self.cfg.display_tz)
    }

    fn holder(&self, class: Class, since: f64, lease: Option<String>) -> Holder {
        Holder {
            pid: self.pid,
            start_ticks: self.start_ticks,
            nonce: self.nonce.clone(),
            class,
            since,
            lease,
        }
    }

    /// True when `h` is the in-flight WRITE of an enclosing gh-paced (see
    /// [`Wrapper::inherited_leases`]).
    fn is_ancestor(&self, h: &Holder) -> bool {
        self.chain.contains(&h.nonce)
            && h.lease
                .as_deref()
                .filter(|n| self.paths.is_lease_name(n))
                .and_then(|n| state::file_id(&self.paths.dir.join(n)))
                .is_some_and(|id| self.inherited_leases.contains(&id))
    }

    fn record(&self, event: &str, c: &Classification, summary: &str) -> Record {
        let mut r = Record::at(self.clock.now(), &self.host, event);
        r.class = c.class.name().to_string();
        r.cost = c.cost;
        r.command = c.command.clone();
        r.argv = summary.to_string();
        r
    }

    fn write_audit(&mut self, r: &Record) {
        if let Err(e) = audit::append(&self.paths.audit(), r) {
            self.loud("WARNING", &format!("audit log not written: {e}"));
        }
    }

    /// Lock, read the clock, load and reap (removing dead holders' lease files). The time is
    /// read after the lock is held, so it is never older than the state it is applied to. The
    /// last value says whether another process held the lock at the first try.
    fn open_state(&mut self) -> Result<(state::LockGuard, State, f64, bool), String> {
        let (guard, contended) = self.lock()?;
        let now = self.clock.now();
        let cfg = self.cfg.clone();
        let loaded = state::load(&self.paths, now, &|t| recovery_state(&cfg, t));
        if let Some(w) = &loaded.warning {
            let w = w.clone();
            self.banner("WARNING", &[w]);
        }
        let mut st = loaded.state;
        for dead in st.reap(self.alive) {
            if let Some(name) = &dead.lease {
                state::remove_lease(&self.paths, name);
            }
        }
        Ok((guard, st, now, contended))
    }

    /// How long to wait for the state lock: GH_PACED_LOCK_WAIT, and for a GIT_CREDENTIAL call
    /// no longer than what is left of its wait bound (0, a single try, once that has passed).
    /// The second value names that bound when it is the shorter one.
    fn lock_wait(&self) -> (f64, Option<String>) {
        let full = self.cfg.lock_wait_secs;
        let Some(left) = self.credential_left() else {
            return (full, None);
        };
        let left = left.max(0.0);
        if left >= full {
            return (full, None);
        }
        (left, Some(self.credential_lock_note(left)))
    }

    /// Seconds left of a GIT_CREDENTIAL call's wait bound by the monotonic clock (0 or less once
    /// it has passed), or `None` for any other call.
    fn credential_left(&self) -> Option<f64> {
        let started = self.credential_started?;
        let (bound, _) = self.cfg.max_wait(Class::GitCredential);
        Some(started + bound - self.clock.monotonic())
    }

    fn credential_lock_note(&self, left: f64) -> String {
        let (bound, var) = self.cfg.max_wait(Class::GitCredential);
        format!(
            "a GIT_CREDENTIAL call waits for it only within its {var}={} s bound, which had {} s \
             left",
            fmt_num(bound),
            fmt_num(left)
        )
    }

    /// Take the state lock within [`Self::lock_wait`]. The second value says whether another
    /// process held it at the first try, so that this one waited for it.
    ///
    /// A GIT_CREDENTIAL call also stops waiting as soon as its bound passes, checked against the
    /// clock on every try: `lock_wait` turns the bound into a number of seconds before the lock
    /// is first tried, and time that passes after that (a stall, a suspend) would otherwise
    /// be added to the bound.
    fn lock(&self) -> Result<(state::LockGuard, bool), String> {
        let (wait, note) = self.lock_wait();
        let left = || self.credential_left().unwrap_or(f64::INFINITY);
        state::lock_within_noting_until(&self.paths, wait, &left).map_err(|e| {
            let note = note.or_else(|| {
                self.credential_left()
                    .filter(|left| *left <= 0.0)
                    .map(|_| self.credential_lock_note(0.0))
            });
            match note {
                Some(n) => format!("{e}; {n}"),
                None => e,
            }
        })
    }

    fn child_env(&self) -> Vec<(String, String)> {
        let mut chain = self.chain.clone();
        chain.push(self.nonce.clone());
        vec![
            ("GH_PACED_INFLIGHT_CHAIN".to_string(), chain.join(",")),
            ("GH_PACED_DEPTH".to_string(), (self.depth + 1).to_string()),
        ]
    }

    fn internal_error(&mut self, what: &str, e: &str) -> Outcome {
        self.banner(
            "ERROR",
            &[
                format!("{what}: {e}"),
                format!("refusing to run gh without its pacing state (exit {EXIT_INTERNAL})"),
            ],
        );
        Outcome::Exit(EXIT_INTERNAL)
    }

    /// Run one gh command line (without the program name).
    pub fn run(&mut self, args: &[String]) -> Outcome {
        let outcome = self.run_inner(args);
        self.flush();
        outcome
    }

    fn run_inner(&mut self, args: &[String]) -> Outcome {
        // A GIT_CREDENTIAL call's wait bound counts from here, before anything waits for the
        // state lock: refusing an alias below is audited under that lock too.
        if classify(args, &self.cfg).class == Class::GitCredential {
            self.credential_started = Some(self.clock.monotonic());
        }
        // Resolve gh's own aliases first, and hand gh the expanded command line, so the command
        // classified, snapshotted and guarded below is the one gh runs, provided gh's
        // configuration is not rewritten before gh reads it again (outside the threat model).
        // A configuration gh-paced cannot read refuses every command here (exit 78).
        let (args, via) = match self.gh_aliases.resolve(args) {
            Resolution::NotAlias => (args.to_vec(), None),
            Resolution::Expanded { argv, chain } => {
                let note = format!(
                    "gh alias `{}` -> `{}`",
                    chain.join("` -> `"),
                    argv.join(" ")
                );
                (argv, Some(note))
            }
            Resolution::Shell { argv, chain } => {
                let note = format!(
                    "gh shell alias `{}`, which gh runs with `sh -c`",
                    chain.join("` -> `")
                );
                (argv, Some(note))
            }
            Resolution::Refused { reason, config } => {
                let code = if config { EXIT_CONFIG } else { EXIT_USAGE };
                let c = classify(args, &self.cfg);
                self.banner(
                    "REFUSED",
                    &[
                        reason.clone(),
                        format!("not running `{}` (exit {code})", args.join(" ")),
                    ],
                );
                let split = c.rest_start.min(args.len());
                let mut shown: Vec<String> = args[..split].to_vec();
                shown.extend(guard::redact(&c, &args[split..]));
                let mut r = self.record("refuse", &c, &audit::summarize(&shown));
                r.rc = Some(code);
                r.detail = format!("gh alias: {reason}");
                if let Err(e) = self.audit_locked(&r) {
                    return self.internal_error("cannot record refusal", &e);
                }
                return Outcome::Exit(code);
            }
        };
        self.alias_note = via.clone();
        let args = args.as_slice();
        let annotate = |mut c: Classification| {
            if let Some(v) = &via {
                c.reason = format!("{} ({v})", c.reason);
            }
            c
        };
        let c0 = annotate(classify(args, &self.cfg));
        if c0.class == Class::Local {
            return self.spawn(args, None, &c0, Vec::new()).0;
        }
        let split = c0.rest_start.min(args.len());
        let mut shown: Vec<String> = args[..split].to_vec();
        shown.extend(guard::redact(&c0, &args[split..]));
        let summary = audit::summarize(&shown);
        // Remove snapshot directories whose invocation has ended (see `snapshot::sweep`).
        snapshot::sweep(&self.paths.dir);
        // Copy every file the command sends, and inspect and send only the copies.
        let (snapshot, args) = match self.take_snapshot(&c0, args, split, &summary) {
            Ok(x) => x,
            Err(code) => return Outcome::Exit(code),
        };
        let args = args.as_slice();
        let c = annotate(classify(args, &self.cfg));
        if c.class == Class::Local {
            return self.spawn(args, None, &c, Vec::new()).0;
        }
        // The command gh will run decides: a GIT_CREDENTIAL call reached through an alias starts
        // its bound here, and any other call has none.
        self.credential_started = if c.class == Class::GitCredential {
            self.credential_started
                .or_else(|| Some(self.clock.monotonic()))
        } else {
            None
        };
        let rest = &args[c.rest_start.min(args.len())..];
        for w in c.warnings.clone() {
            self.loud("WARNING", &w);
        }
        if let Some(refusal) = c.refusal.clone() {
            self.banner(
                "REFUSED",
                &[
                    refusal.clone(),
                    format!("not running `{}` (exit {EXIT_USAGE})", c.command),
                ],
            );
            let mut r = self.record("refuse", &c, &summary);
            r.rc = Some(EXIT_USAGE);
            r.detail = refusal;
            if let Err(e) = self.audit_locked(&r) {
                return self.internal_error("cannot record refusal", &e);
            }
            return Outcome::Exit(EXIT_USAGE);
        }
        let stdin = match self.content_guard(&c, rest, &summary) {
            Ok(s) => s,
            Err(code) => return Outcome::Exit(code),
        };
        let admitted = match self.admit(&c, &summary) {
            Ok(a) => a,
            Err(code) => return Outcome::Exit(code),
        };
        let keep_fds: Vec<RawFd> = admitted
            .lease
            .as_ref()
            .map(Lease::fd)
            .into_iter()
            .chain(snapshot.lock_fd())
            .collect();
        let (outcome, scanner, ran) = self.spawn(args, stdin, &c, keep_fds);
        if scanner.verdict(&self.cfg).is_some() {
            // The cooldown GitHub asked for must reach the state file: its bookkeeping waits
            // for the lock the whole GH_PACED_LOCK_WAIT, not what is left of a GIT_CREDENTIAL
            // call's bound. gh has run, so git is not told to quit either way.
            self.credential_started = None;
        }
        if let Err(e) = self.finish(&c, &summary, ran, &scanner, admitted) {
            self.loud("ERROR", &format!("bookkeeping after gh exited failed: {e}"));
        }
        if ran.is_some_and(|r| r.deadline_hit) {
            return Outcome::Exit(EXIT_REFUSED);
        }
        outcome
    }

    /// Copy the files `c0` sends (see [`snapshot`]) and return the argument list naming the
    /// copies. Nothing is created when the command sends no file.
    fn take_snapshot(
        &mut self,
        c0: &Classification,
        args: &[String],
        split: usize,
        summary: &str,
    ) -> Result<(snapshot::Snapshot, Vec<String>), i32> {
        let rest0 = &args[split..];
        let files = guard::file_sources(c0, rest0);
        if files.is_empty() {
            return Ok((snapshot::Snapshot::default(), args.to_vec()));
        }
        let taken = state::ensure_dir(&self.paths.dir)
            .and_then(|()| snapshot::take(&self.paths.dir, &self.nonce, &files, rest0));
        match taken {
            Ok((snap, new_rest)) => {
                let mut out = args[..split].to_vec();
                out.extend(new_rest);
                Ok((snap, out))
            }
            Err(e) => {
                let reason = format!("cannot copy a file the command sends for inspection: {e}");
                self.refuse_content(c0, summary, &reason);
                Err(EXIT_CONTENT)
            }
        }
    }

    fn audit_locked(&mut self, r: &Record) -> Result<(), String> {
        let _guard = self.lock()?;
        self.write_audit(r);
        Ok(())
    }

    fn spawn(
        &mut self,
        args: &[String],
        stdin: Option<Vec<u8>>,
        c: &Classification,
        keep_fds: Vec<RawFd>,
    ) -> (Outcome, Scanner, Option<Ran>) {
        // Every warning before gh's own output, and never from under the lock.
        self.flush();
        // One response (no --paginate): only the header block on its first stdout line is read.
        let mut scanner = if c.api.as_ref().is_some_and(|a| a.include && !a.paginate) {
            Scanner::single_response()
        } else {
            Scanner::new()
        };
        let local = c.class == Class::Local;
        let mut env = if local { Vec::new() } else { self.child_env() };
        if c.class == Class::Write && !self.cfg.allow_large_body {
            env.extend(self.editor_guard_env.iter().cloned());
            env.push((
                crate::editor::PRIOR_BYTES_VAR.to_string(),
                self.body_bytes_used.to_string(),
            ));
        }
        let real_gh = self.real_gh.clone();
        // Recording a cooldown GitHub asked for is never cut short, for any class: it may wait
        // the whole GH_PACED_LOCK_WAIT, as finish() does when gh's output showed pushback.
        let hook_wait = self.cfg.lock_wait_secs;
        let on_pushback = if local {
            None
        } else {
            let (paths, cfg, command) = (self.paths.clone(), self.cfg.clone(), c.command.clone());
            Some(PushbackHook(Arc::new(move |s: &Scanner| {
                if let Some(pb) = s.verdict(&cfg) {
                    // Errors surface later: finish() records the same cooldown and reports them.
                    let _ = publish_cooldown(&paths, hook_wait, RealClock.now(), &pb, &command);
                }
            })))
        };
        // The drainer records pushback in what it delivers against this call's command; a
        // LOCAL call records none, as here.
        let drain = self.self_exe.clone().map(|program| DrainCommand {
            program,
            args: vec![
                DRAIN_FLAG.to_string(),
                self.paths.account.clone(),
                if local {
                    String::new()
                } else {
                    c.command.clone()
                },
            ],
        });
        let inv = Invocation {
            program: &real_gh,
            args,
            stdin,
            env,
            deadline_secs: if local { None } else { c.deadline_secs },
            keep_fds,
            scan_stdout: !local && c.api.as_ref().is_some_and(|a| a.include),
            on_pushback,
            hook_wait_secs: hook_wait,
            drain,
        };
        // Taken as started unless the runner says gh's program never ran.
        self.gh_started = true;
        match self.runner.run(inv, &mut scanner) {
            Ok(ran) => {
                if ran.cut_off {
                    self.loud(
                        "WARNING",
                        "a process gh started was still holding gh's output when gh-paced \
                         stopped reading it, and no drainer could take it over: anything it \
                         wrote afterwards was lost",
                    );
                }
                if ran.late_signal.is_some() {
                    // The caller asked gh-paced to stop while gh's output was still being
                    // delivered, so stderr may be a stalled consumer. Every later message
                    // (warnings, the PUSHBACK banner, the exit line) gets a bounded write,
                    // however gh itself ended.
                    self.stderr_deadline =
                        Some(Instant::now() + Duration::from_secs_f64(LATE_SIGNAL_STDERR_SECS));
                }
                let outcome = match (ran.exit, ran.late_signal) {
                    (Exit::Signal(s), _) => Outcome::Signal(s),
                    (Exit::Code(n), Some(sig)) => {
                        self.loud(
                            "WARNING",
                            &format!(
                                "gh exited {n}; signal {sig} arrived while its output was still \
                                 being delivered, so the rest of that output was dropped"
                            ),
                        );
                        Outcome::Signal(sig)
                    }
                    (Exit::Code(n), None) => Outcome::Exit(n),
                };
                (outcome, scanner, Some(ran))
            }
            Err(e) => {
                self.gh_started = e.started;
                self.loud("ERROR", &e.message);
                (Outcome::Exit(EXIT_NO_GH), scanner, None)
            }
        }
    }

    /// Print and audit a content-guard refusal.
    fn refuse_content(&mut self, c: &Classification, summary: &str, reason: &str) {
        self.banner(
            "REFUSED",
            &[
                format!("content guard: {reason}"),
                format!("not running `{}` (exit {EXIT_CONTENT})", c.command),
                "GitHub text is for short human notes. Keep evidence on the host and post a pointer \
                 (path + sha256, a tracked file, or a commit)."
                    .to_string(),
            ],
        );
        let mut r = self.record("refuse", c, summary);
        r.rc = Some(EXIT_CONTENT);
        r.detail = format!("content guard: {reason}");
        if self.audit_locked(&r).is_err() {
            self.loud("WARNING", "audit log not written for the content refusal");
        }
    }

    fn content_guard(
        &mut self,
        c: &Classification,
        rest: &[String],
        summary: &str,
    ) -> Result<Option<Vec<u8>>, i32> {
        if c.class != Class::Write {
            return Ok(None);
        }
        if self.cfg.allow_large_body {
            self.loud(
                "WARNING",
                &format!(
                    "GH_PACED_ALLOW_LARGE_BODY=1: content guard skipped for `{}`",
                    c.command
                ),
            );
            return Ok(None);
        }
        if let Some(why) = guard::uninspectable(c, rest, self.stdin_is_tty) {
            let reason = format!(
                "{why}, after this guard would have run, so the body cannot be inspected; \
                 pass it with --body or --body-file instead"
            );
            self.refuse_content(c, summary, &reason);
            return Err(EXIT_CONTENT);
        }
        let sources = guard::body_sources(c, rest, self.stdin_is_tty);
        let mut stdin_buf = None;
        if sources.iter().any(guard::BodySource::is_stdin) {
            self.flush();
            match (self.stdin_reader)(self.cfg.max_body_bytes + 1) {
                Ok(buf) => stdin_buf = Some(buf),
                Err(e) => {
                    self.loud(
                        "REFUSED",
                        &format!("cannot read stdin for the content guard: {e}"),
                    );
                    return Err(EXIT_CONTENT);
                }
            }
        }
        match guard::evaluate(&sources, &self.cfg, stdin_buf.as_deref()) {
            Verdict::Refuse(reason) => {
                self.refuse_content(c, summary, &reason);
                return Err(EXIT_CONTENT);
            }
            Verdict::Allow { bytes } => self.body_bytes_used = bytes as u64,
        }
        Ok(stdin_buf)
    }

    /// Decide, under the lock, whether this pass refreshes the account-wide snapshot. A refresh
    /// is due when the snapshot is missing, older than `rate_limit_refresh_secs`, or
    /// `rate_limit_refresh_calls` paced calls old; it is never attempted during a cooldown, more
    /// often than `rate_limit_min_refresh_secs`, or while another live process holds the claim.
    /// When the READ budget has no token for it, the call waits for one (subject to
    /// `GH_PACED_MAX_WAIT`) instead of running without the feedback: after a pushback the
    /// snapshot is discarded, and WRITE or SEARCH calls must not resume unchecked just because
    /// READ is busy. Claiming charges that READ token at once.
    fn refresh_step(&mut self, c: &Classification, st: &mut State, now: f64) -> RefreshStep {
        if !paced_api(c.class) || c.api.as_ref().is_some_and(|a| a.endpoint == "rate_limit") {
            return RefreshStep::Skip;
        }
        if st.cooldown.as_ref().is_some_and(|cd| cd.until > now) {
            return RefreshStep::Skip;
        }
        let age = st.rate_limit.as_ref().map(|s| now - s.fetched_at);
        let due = age.is_none_or(|a| a >= self.cfg.rate_limit_refresh_secs)
            || st.calls_since_refresh >= self.cfg.rate_limit_refresh_calls;
        if !due {
            return RefreshStep::Skip;
        }
        if st.refresh_claim.is_some() {
            return if st.rate_limit.is_none() {
                RefreshStep::WaitForOther
            } else {
                RefreshStep::Skip
            };
        }
        if now - st.last_refresh_attempt < self.cfg.rate_limit_min_refresh_secs {
            return RefreshStep::Skip;
        }
        let base = self.cfg.read;
        let limits = match st
            .rate_limit
            .as_ref()
            .and_then(|s| s.pressure(Class::Read, now))
        {
            Some(p) if p.fraction <= self.cfg.halve_below_fraction => halved(base),
            _ => base,
        };
        let bucket = st
            .buckets
            .entry(Class::Read.name().to_string())
            .or_insert_with(|| Bucket::full(base, now));
        bucket.refresh(limits, now);
        let wait = bucket
            .bucket_wait(limits, 1)
            .max(bucket.hour_wait(limits, 1, now));
        if wait > 0.0 {
            return RefreshStep::WaitForRead(wait);
        }
        bucket.charge(1, now);
        st.refresh_claim = Some(self.holder(Class::Read, now, None));
        st.last_refresh_attempt = now;
        RefreshStep::Run
    }

    /// Run the claimed refresh (outside the lock), then record its result under the lock.
    fn run_refresh(&mut self) -> Result<(), String> {
        self.flush();
        let args = vec!["api".to_string(), "rate_limit".to_string()];
        let real_gh = self.real_gh.clone();
        let result = self.runner.capture(
            Invocation {
                program: &real_gh,
                args: &args,
                stdin: None,
                env: self.child_env(),
                deadline_secs: None,
                keep_fds: Vec::new(),
                scan_stdout: false,
                on_pushback: None,
                hook_wait_secs: 0.0,
                drain: None,
            },
            self.cfg.rate_limit_timeout_secs,
        );
        let (_guard, mut st, now, _) = self.open_state()?;
        if st
            .refresh_claim
            .as_ref()
            .is_some_and(|h| h.nonce == self.nonce)
        {
            st.refresh_claim = None;
        }
        let rc_class = Classification {
            class: Class::Read,
            cost: 1,
            family: "api".into(),
            sub: None,
            command: "api GET rate_limit".into(),
            reason: "rate-limit refresh".into(),
            warnings: Vec::new(),
            refusal: None,
            api: None,
            rest_start: 1,
            deadline_secs: None,
            opaque: false,
        };
        let mut r = self.record("refresh", &rc_class, "api rate_limit");
        match result {
            Ok(cap) => {
                let mut sc = Scanner::new();
                sc.feed(&cap.stderr);
                if cap.stderr_cut {
                    // A Retry-After running up to the cut may have had more digits.
                    sc.stderr_cut_off();
                }
                let cut: Vec<&str> = [(cap.stdout_cut, "stdout"), (cap.stderr_cut, "stderr")]
                    .into_iter()
                    .filter_map(|(cut, name)| cut.then_some(name))
                    .collect();
                let cut_note = (!cut.is_empty()).then(|| {
                    format!(
                        "gh's {} ran past the {MAX_CAPTURE_BYTES} bytes gh-paced reads and was \
                         cut off there",
                        cut.join(" and ")
                    )
                });
                if let Some(note) = &cut_note {
                    self.loud("WARNING", &format!("rate-limit refresh: {note}"));
                }
                if let Some(pb) = sc.verdict(&self.cfg) {
                    self.apply_pushback(&mut st, &pb, "api GET rate_limit (gh-paced refresh)", now);
                    r.detail = format!("pushback: {}", pb.reason);
                } else if cap.exit == Exit::Code(0) && !cap.timed_out {
                    match ratelimit::parse(&cap.stdout, now) {
                        Ok(snap) => {
                            if let Some(p) = snap.pressure(Class::Read, now) {
                                r.detail = format!(
                                    "{} {}/{} left ({:.0}%)",
                                    p.resource,
                                    p.remaining,
                                    p.limit,
                                    p.fraction * 100.0
                                );
                            }
                            st.rate_limit = Some(snap);
                            st.calls_since_refresh = 0;
                        }
                        Err(e) => {
                            r.detail = format!("unparseable: {e}");
                            self.loud(
                                "WARNING",
                                &format!("rate-limit refresh output unusable ({e}); pacing on local budgets only"),
                            );
                        }
                    }
                } else {
                    let why = if cap.timed_out {
                        format!(
                            "timed out after {} s",
                            fmt_num(self.cfg.rate_limit_timeout_secs)
                        )
                    } else {
                        format!("exit {:?}", cap.exit)
                    };
                    r.detail = format!("failed: {why}");
                    self.loud(
                        "WARNING",
                        &format!("rate-limit refresh failed ({why}); pacing on local budgets only"),
                    );
                }
                if let Some(note) = cut_note {
                    r.detail = if r.detail.is_empty() {
                        note
                    } else {
                        format!("{}; {note}", r.detail)
                    };
                }
            }
            Err(e) => {
                r.detail = format!("failed: {e}");
                self.loud(
                    "WARNING",
                    &format!(
                        "rate-limit refresh could not start ({e}); pacing on local budgets only"
                    ),
                );
            }
        }
        self.write_audit(&r);
        state::save(&self.paths, &st)
    }

    fn apply_pushback(&mut self, st: &mut State, pb: &Pushback, command: &str, now: f64) {
        let until = pb.until(now);
        let extended = match &st.cooldown {
            Some(cd) if cd.until >= until => false,
            _ => {
                st.cooldown = Some(Cooldown {
                    until,
                    set_at: now,
                    reason: pb.reason.clone(),
                    command: command.to_string(),
                });
                true
            }
        };
        st.rate_limit = None;
        if extended {
            if let Some(cd) = &st.cooldown {
                if let Err(e) = state::save_cooldown(&self.paths, cd) {
                    self.loud(
                        "WARNING",
                        &format!("cooldown record not written (the state file still has it): {e}"),
                    );
                }
            }
        }
        let effective = st.cooldown.as_ref().map(|c| c.until).unwrap_or(until);
        let pause =
            if has_no_end(effective) {
                format!(
                "every paced gh call for this account on this host is refused from now on: the \
                 cooldown has no end time. It ends only when a person removes {} and the \
                 `cooldown` entry of {}, after checking with GitHub",
                self.paths.cooldown().display(),
                self.paths.state().display()
            )
            } else {
                format!(
                "every paced gh call for this account on this host is paused for {} s, until {}{}",
                (effective - now).ceil() as i64,
                self.when(effective),
                if extended { "" } else { " (an existing longer cooldown stays)" }
            )
            };
        self.banner(
            "PUSHBACK",
            &[
                format!("GitHub refused or throttled `{command}`: {}", pb.reason),
                pause,
                "STOP making GitHub calls and tell your coordinator. Do not retry in a loop. \
                 Never switch accounts to continue."
                    .to_string(),
            ],
        );
    }

    /// Wait until the call fits every budget, then charge it. A WRITE also takes the in-flight
    /// slot and its lease. Each pass takes the lock, reads the clock, refreshes the account
    /// snapshot first when that is due, and either admits or sleeps for the longest wait.
    fn admit(&mut self, c: &Classification, summary: &str) -> Result<Admitted, i32> {
        let class = c.class;
        let base = self.cfg.limits(class);
        let (max_wait, max_wait_var) = self.cfg.max_wait(class);
        // A GIT_CREDENTIAL call's stay is measured on the monotonic clock, so that setting the
        // system time back cannot lengthen it; budgets and cooldowns keep using wall time.
        let started = self
            .credential_started
            .unwrap_or_else(|| self.clock.monotonic());
        let mut waited = 0.0;
        // Whether any pass found the state lock held by another process and waited for it.
        let mut lock_waited = false;
        // Whether any pass decided to sleep for a budget (even if, for a GIT_CREDENTIAL call,
        // its bound passed before the sleep began, so that it did not sleep).
        let mut throttled = false;
        let mut halve_noted = false;
        loop {
            let (guard, mut st, now, contended) = match self.open_state() {
                Ok(x) => x,
                Err(e) => {
                    self.internal_error("cannot open pacing state", &e);
                    return Err(EXIT_INTERNAL);
                }
            };
            lock_waited |= contended;
            let stayed = self.clock.monotonic() - started;
            let mut waits: Vec<Wait> = Vec::new();
            match self.refresh_step(c, &mut st, now) {
                RefreshStep::Run => {
                    let saved = state::save(&self.paths, &st);
                    drop(guard);
                    if let Err(e) = saved.and_then(|()| self.run_refresh()) {
                        self.internal_error("cannot refresh the rate-limit snapshot", &e);
                        return Err(EXIT_INTERNAL);
                    }
                    continue;
                }
                RefreshStep::WaitForOther => waits.push(Wait {
                    secs: REFRESH_POLL_SECS,
                    kind: WaitKind::Refresh,
                    text: "another gh-paced process is fetching the account-wide rate-limit \
                           snapshot and there is none yet"
                        .to_string(),
                }),
                RefreshStep::WaitForRead(secs) => waits.push(Wait {
                    secs,
                    kind: WaitKind::RefreshBudget,
                    text: "the account-wide rate-limit snapshot is due and the read budget has \
                           no token for the request that fetches it; calls do not run without \
                           current account feedback"
                        .to_string(),
                }),
                RefreshStep::Skip => {}
            }
            let pressure = if paced_api(class) {
                st.rate_limit.as_ref().and_then(|s| s.pressure(class, now))
            } else {
                None
            };
            let halve = pressure
                .as_ref()
                .is_some_and(|p| p.fraction <= self.cfg.halve_below_fraction);
            let limits = if halve { halved(base) } else { base };
            if halve && !halve_noted {
                if let Some(p) = &pressure {
                    let text = format!(
                        "account-wide {} budget at {:.0}% ({}/{} left until {}); local {} rates halved to {} and {}/hour",
                        p.resource,
                        p.fraction * 100.0,
                        p.remaining,
                        p.limit,
                        self.when(p.reset),
                        class.name(),
                        describe_rate(limits),
                        limits.per_hour
                    );
                    self.loud("WARNING", &text);
                }
                halve_noted = true;
            }
            // One gh call makes all the requests its cost stands for back to back (pages of a
            // list, --paginate), and gh-paced cannot space out requests inside one gh call. A
            // cost above the burst would therefore exceed the burst however long the call
            // waited, so it is refused. A watch is the exception: its requests are spread over
            // its poll interval, it is admitted on a full bucket, and the rest of its cost is
            // charged as debt that later calls repay.
            if c.deadline_secs.is_none() && f64::from(c.cost) > limits.burst + crate::budget::EPS {
                if let Err(e) = state::save(&self.paths, &st) {
                    drop(guard);
                    self.internal_error("cannot save pacing state", &e);
                    return Err(EXIT_INTERNAL);
                }
                let text = format!(
                    "`{}` costs {} {} tokens, more than the {} burst of {}{}: one gh call makes \
                     those requests back to back, and gh-paced cannot space them out",
                    c.command,
                    c.cost,
                    class.name(),
                    class.name(),
                    fmt_num(limits.burst),
                    if halve {
                        " (halved because the account-wide budget is low)"
                    } else {
                        ""
                    }
                );
                self.banner(
                    "REFUSED",
                    &[
                        text.clone(),
                        format!(
                            "lower --limit, drop --paginate, split the work into smaller calls, \
                             or raise the {} burst in the config file",
                            class.name()
                        ),
                        format!("not running `{}` (exit {EXIT_REFUSED})", c.command),
                    ],
                );
                let mut r = self.record("refuse", c, summary);
                r.rc = Some(EXIT_REFUSED);
                r.waited_secs = waited;
                r.detail = text;
                self.write_audit(&r);
                return Err(EXIT_REFUSED);
            }
            if let Some(cd) = &st.cooldown {
                if cd.until > now {
                    let text = if cd.command == RECOVERY_COMMAND {
                        format!("pause after state recovery: {}", cd.reason)
                    } else {
                        format!(
                            "cooldown after GitHub pushback ({}) on `{}`",
                            cd.reason, cd.command
                        )
                    };
                    waits.push(Wait {
                        secs: cd.until - now,
                        kind: WaitKind::Cooldown,
                        text,
                    });
                }
            }
            if let Some(p) = &pressure {
                if p.fraction <= self.cfg.block_below_fraction {
                    waits.push(Wait {
                        secs: (p.reset - now).max(1.0),
                        kind: WaitKind::Block,
                        text: format!(
                            "account-wide {} budget at {:.0}% ({}/{} left), at or below the {:.0}% floor; blocking until it resets",
                            p.resource,
                            p.fraction * 100.0,
                            p.remaining,
                            p.limit,
                            self.cfg.block_below_fraction * 100.0
                        ),
                    });
                }
            }
            if class == Class::Write {
                let others: Vec<u32> = st
                    .in_flight
                    .iter()
                    .filter(|h| h.class == Class::Write && !self.is_ancestor(h))
                    .map(|h| h.pid)
                    .collect();
                if u32::try_from(others.len()).unwrap_or(u32::MAX) >= self.cfg.write_max_in_flight {
                    let pids: Vec<String> = others.iter().map(u32::to_string).collect();
                    waits.push(Wait {
                        secs: IN_FLIGHT_POLL_SECS,
                        kind: WaitKind::InFlight,
                        text: format!(
                            "{} write(s) already in flight on this host (pid {}), limit {}",
                            others.len(),
                            pids.join(", "),
                            self.cfg.write_max_in_flight
                        ),
                    });
                }
            }
            let bucket = st
                .buckets
                .entry(class.name().to_string())
                .or_insert_with(|| Bucket::full(base, now));
            bucket.refresh(limits, now);
            let bw = bucket.bucket_wait(limits, c.cost);
            if bw > 0.0 {
                waits.push(Wait {
                    secs: bw,
                    kind: WaitKind::Bucket,
                    text: format!(
                        "{} budget {} (burst {}) per host reached",
                        class.name(),
                        describe_rate(limits),
                        fmt_num(limits.burst)
                    ),
                });
            }
            let hw = bucket.hour_wait(limits, c.cost, now);
            if hw > 0.0 {
                let blocked = bucket.blocked_wait(now);
                let text = if hw.is_finite() && blocked > 0.0 && blocked + 1e-6 >= hw {
                    format!(
                        "{} budget blocked until {} after state recovery (the lost history may \
                         have held a full hour of calls)",
                        class.name(),
                        self.when(now + blocked)
                    )
                } else {
                    format!(
                        "{} budget {}/hour per host reached ({} used in the last hour)",
                        class.name(),
                        limits.per_hour,
                        bucket.hour_used()
                    )
                };
                waits.push(Wait {
                    secs: hw,
                    kind: WaitKind::Hour,
                    text,
                });
            }
            let worst = waits
                .iter()
                .max_by(|a, b| a.secs.total_cmp(&b.secs))
                .cloned();
            let Some(w) = worst else {
                if class == Class::GitCredential && (throttled || lock_waited) && stayed > max_wait
                {
                    // It became admissible only after the bound had passed: a late wake-up, a
                    // bound that passed before a sleep began, or the state lock taken just at
                    // or after the bound. A call that has waited for nothing is admitted
                    // however long its own work took, so a bound of 0 still runs a call whose
                    // token is there and whose lock is free.
                    if let Err(e) = state::save(&self.paths, &st) {
                        drop(guard);
                        self.internal_error("cannot save pacing state", &e);
                        return Err(EXIT_INTERNAL);
                    }
                    let text = format!(
                        "`{}` became admissible {} s after it started, beyond {max_wait_var}={} s",
                        c.command,
                        fmt_num(stayed),
                        fmt_num(max_wait)
                    );
                    self.banner(
                        "REFUSED",
                        &[
                            text.clone(),
                            format!("not running `{}` (exit {EXIT_REFUSED})", c.command),
                        ],
                    );
                    let mut r = self.record("refuse", c, summary);
                    r.rc = Some(EXIT_REFUSED);
                    r.waited_secs = waited;
                    r.detail = text;
                    self.write_audit(&r);
                    return Err(EXIT_REFUSED);
                }
                let lease = if class == Class::Write {
                    match state::create_lease(&self.paths, &self.nonce) {
                        Ok(l) => Some(l),
                        Err(e) => {
                            drop(guard);
                            self.internal_error("cannot take the write lease", &e);
                            return Err(EXIT_INTERNAL);
                        }
                    }
                } else {
                    None
                };
                bucket.charge(c.cost, now);
                if let Some(l) = &lease {
                    let h = self.holder(class, now, Some(l.name.clone()));
                    st.in_flight.push(h);
                }
                if class != Class::GitCredential {
                    st.calls_since_refresh = st.calls_since_refresh.saturating_add(1);
                }
                if let Err(e) = state::save(&self.paths, &st) {
                    if let Some(l) = lease {
                        state::remove_lease(&self.paths, &l.name);
                    }
                    drop(guard);
                    self.internal_error("cannot save pacing state", &e);
                    return Err(EXIT_INTERNAL);
                }
                let mut r = self.record("admit", c, summary);
                r.waited_secs = waited;
                self.write_audit(&r);
                return Ok(Admitted { waited, lease });
            };
            if let Err(e) = state::save(&self.paths, &st) {
                drop(guard);
                self.internal_error("cannot save pacing state", &e);
                return Err(EXIT_INTERNAL);
            }
            if class == Class::GitCredential {
                // A GIT_CREDENTIAL call never waits out a cooldown, however little of it is
                // left: git runs the helper inside a fetch or push that may hold its caller's
                // locks for as long as the helper sleeps.
                if let Some(cd) = waits.iter().find(|x| x.kind == WaitKind::Cooldown) {
                    let cd = cd.clone();
                    return Err(self.refuse_credential_in_cooldown(c, summary, &cd, now, waited));
                }
            }
            if !w.secs.is_finite() {
                let text = format!(
                    "`{}` costs {} {} tokens, more than the whole hourly cap of {}{}; it can never be admitted",
                    c.command,
                    c.cost,
                    class.name(),
                    limits.per_hour,
                    if halve {
                        " (halved because the account-wide budget is low)"
                    } else {
                        ""
                    }
                );
                self.banner(
                    "REFUSED",
                    &[
                        text.clone(),
                        "lower --limit, drop --paginate, or split the work into smaller calls"
                            .to_string(),
                        format!("not running `{}` (exit {EXIT_REFUSED})", c.command),
                    ],
                );
                let mut r = self.record("refuse", c, summary);
                r.rc = Some(EXIT_REFUSED);
                r.waited_secs = waited;
                r.detail = text;
                self.write_audit(&r);
                return Err(EXIT_REFUSED);
            }
            // A GIT_CREDENTIAL call also counts time spent waiting for the state lock, so the
            // helper's whole stay stays near its short bound.
            let spent = if class == Class::GitCredential {
                waited.max(stayed)
            } else {
                waited
            };
            if spent + w.secs > max_wait {
                let until = now + w.secs;
                let wait_text = if w.kind.polled() {
                    format!("waited {} s for it", waited.ceil() as i64)
                } else {
                    format!(
                        "the next slot is in {} s (at {})",
                        w.secs.ceil() as i64,
                        self.when(until)
                    )
                };
                let limit = if !w.kind.polled() && has_no_end(until) {
                    format!(
                        "the cooldown has no end time: every paced call is refused until a \
                         person removes {} and the `cooldown` entry of {}",
                        self.paths.cooldown().display(),
                        self.paths.state().display()
                    )
                } else {
                    format!("{wait_text}, beyond {max_wait_var}={} s", fmt_num(max_wait))
                };
                self.banner(
                    "REFUSED",
                    &[
                        w.text.clone(),
                        limit,
                        format!("not running `{}` (exit {EXIT_REFUSED})", c.command),
                    ],
                );
                let mut r = self.record("refuse", c, summary);
                r.rc = Some(EXIT_REFUSED);
                r.waited_secs = waited;
                r.detail = w.text.clone();
                self.write_audit(&r);
                return Err(EXIT_REFUSED);
            }
            let text = if w.kind.polled() {
                format!(
                    "{}; waiting (re-checking every {} s, waited {} s so far)",
                    w.text,
                    fmt_num(w.secs),
                    waited.ceil() as i64
                )
            } else {
                format!(
                    "{}; sleeping {} s (next slot {})",
                    w.text,
                    w.secs.ceil() as i64,
                    self.when(now + w.secs)
                )
            };
            self.loud("WARNING", &text);
            let mut r = self.record("throttle", c, summary);
            r.waited_secs = waited;
            r.detail = w.text.clone();
            self.write_audit(&r);
            drop(guard);
            self.flush();
            throttled = true;
            if class == Class::GitCredential {
                // The sleep ends at a deadline on the clock the bound is measured on: `w.secs`
                // from this reading, taken after the state, the audit record and stderr were
                // written, and never past the bound. A deadline rather than a duration, so that
                // neither a stall or suspend after this reading nor a suspend during the sleep
                // moves the wake-up later. A call whose bound has passed does not sleep, and the
                // next pass refuses it.
                let from = self.clock.monotonic();
                let until = (from + w.secs).min(started + max_wait);
                if let Err(e) = self.clock.sleep_until_monotonic(until) {
                    self.banner(
                        "ERROR",
                        &[
                            format!("cannot wait on the clock {max_wait_var} is measured on: {e}"),
                            format!("not running `{}` (exit {EXIT_INTERNAL})", c.command),
                        ],
                    );
                    return Err(EXIT_INTERNAL);
                }
                waited += (until - from).max(0.0);
            } else {
                self.clock.sleep(w.secs);
                waited += w.secs;
            }
        }
    }

    /// Refuse a GIT_CREDENTIAL call at once because the account is in a cooldown (or the pause
    /// after state recovery), with one loud line naming the cause and the seconds left, and
    /// record the refusal. Returns the exit status.
    fn refuse_credential_in_cooldown(
        &mut self,
        c: &Classification,
        summary: &str,
        cd: &Wait,
        now: f64,
        waited: f64,
    ) -> i32 {
        let until = now + cd.secs;
        let left = if has_no_end(until) {
            format!(
                "the cooldown has no end time: every paced call is refused until a person \
                 removes {} and the `cooldown` entry of {}",
                self.paths.cooldown().display(),
                self.paths.state().display()
            )
        } else {
            format!(
                "{} s left (ends {})",
                cd.secs.ceil() as i64,
                self.when(until)
            )
        };
        let text = format!(
            "{}; {left}; a git credential helper never waits out a cooldown, so git fails now \
             instead of holding its caller's locks; not running `{}` (exit {EXIT_REFUSED})",
            cd.text, c.command
        );
        self.loud("REFUSED", &text);
        let mut r = self.record("refuse", c, summary);
        r.rc = Some(EXIT_REFUSED);
        r.waited_secs = waited;
        r.detail = cd.text.clone();
        self.write_audit(&r);
        EXIT_REFUSED
    }

    fn finish(
        &mut self,
        c: &Classification,
        summary: &str,
        ran: Option<Ran>,
        scanner: &Scanner,
        admitted: Admitted,
    ) -> Result<(), String> {
        let (_guard, mut st, now, _) = self.open_state()?;
        let nonce = self.nonce.clone();
        // Close this process's copy of the lease first, then ask whether anyone else still holds
        // it. gh has exited, but a process it started (a backgrounded helper, an alias's shell)
        // may still hold the inherited descriptor and still be writing: its slot stays taken,
        // with the holder entry and lease file kept, until it lets go and a later call reaps it.
        let mut retained = false;
        if let Some(lease) = admitted.lease {
            let name = lease.name.clone();
            drop(lease);
            if state::lease_held(&self.paths.dir, &name) {
                retained = true;
                self.loud(
                    "WARNING",
                    &format!(
                        "gh exited but a process it started still holds this write's slot \
                         (lease {name} is locked); other writes on this host wait until that \
                         process exits"
                    ),
                );
            } else {
                state::remove_lease(&self.paths, &name);
            }
        }
        if !retained {
            st.in_flight.retain(|h| h.nonce != nonce);
        }
        if let Some(pb) = scanner.verdict(&self.cfg) {
            self.apply_pushback(&mut st, &pb, &c.command, now);
            let mut r = self.record("pushback", c, summary);
            r.detail = pb.reason.clone();
            self.write_audit(&r);
        }
        if let Some(limit) = c
            .deadline_secs
            .filter(|_| ran.is_some_and(|r| r.deadline_hit))
        {
            let text = format!(
                "`{}` ran past its {} s budget (its cost of {} tokens is estimated to cover \
                 that many polls at the interval given) and was stopped",
                c.command,
                limit.ceil() as i64,
                c.cost
            );
            self.banner(
                "DEADLINE",
                &[
                    text.clone(),
                    format!("exit {EXIT_REFUSED}; re-run it if it still needs watching"),
                ],
            );
            let mut r = self.record("deadline", c, summary);
            r.detail = text;
            r.rc = Some(EXIT_REFUSED);
            self.write_audit(&r);
        }
        let mut r = self.record("exit", c, summary);
        r.waited_secs = admitted.waited;
        match ran.map(|r| r.exit) {
            Some(Exit::Code(n)) => r.rc = Some(n),
            Some(Exit::Signal(s)) => r.signal = Some(s),
            None => r.rc = Some(EXIT_NO_GH),
        }
        self.write_audit(&r);
        state::save(&self.paths, &st)
    }
}
