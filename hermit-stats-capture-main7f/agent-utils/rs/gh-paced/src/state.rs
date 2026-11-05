//! Per-account pacing state: one JSON file per account, guarded by an exclusive `flock`.
//!
//! Layout under the state directory (default `~/.local/state/gh-paced/`, mode 0700):
//!
//! - `<account>.json`: buckets, hourly windows, in-flight writes, cooldown, rate-limit snapshot;
//! - `<account>.lock`: the lock file every process takes before reading or writing the JSON;
//! - `<account>.audit.jsonl`: the append-only audit log (see `audit`);
//! - `<account>.cooldown`: a copy of the latest pushback cooldown, written separately so that a
//!   damaged state file cannot shorten a long Retry-After pause (every load takes the later of
//!   the two);
//! - `<account>.lease-<nonce>`: one file per running WRITE (see "Write leases" below);
//! - `snap-<pid>-<ticks>-<nonce>/`: private copies of the body files one running invocation
//!   sends (see `snapshot`).
//!
//! The lock is held only for short read-modify-write steps, never across a sleep or a network
//! call. Saving writes a temporary file and renames it over the old one, so a crash leaves
//! either the old or the new state, and a reader without the lock (`status`) always sees one
//! whole file. A file that cannot be parsed is kept for inspection as
//! `<account>.json.corrupt-<unix-seconds>` (a hard link, else a copy) and replaced, at once and
//! by an atomic rename, by the caller's recovery state (the wrapper blocks every class for an
//! hour and adds a pause, see `wrapper::recovery_state`), with every still-held write lease
//! restored as an in-flight write. The state path never goes missing in between, so a crash
//! during recovery leaves either the damaged file (recovered again by the next caller) or the
//! saved recovery state, never a fresh start.
//!
//! # Write leases
//!
//! A WRITE's in-flight slot must last as long as the gh process doing the write, not as long as
//! the wrapper: if the wrapper is killed with SIGKILL, gh keeps running. So an admitted WRITE
//! creates `<account>.lease-<nonce>`, takes an exclusive `flock` on it, and hands that open file
//! to gh (the descriptor is inherited across exec). The kernel keeps the lock while ANY process
//! still has that open file: the wrapper, gh, or anything gh started. A slot is live while its
//! lease is locked; another process tests that with a non-blocking shared `flock` on a fresh open.
//! A nested gh-paced (an alias or extension calling gh) proves it descends from the holder by
//! holding the holder's locked open file itself: Linux lists a `flock` in
//! `/proc/self/fdinfo/<fd>` only for the open file description that took it, so an independent
//! open of the lease file (for example stdin redirected from it) proves nothing (see
//! `inherited_locked_file_ids`).

use crate::budget::Bucket;
use crate::classify::Class;
use crate::ratelimit::Snapshot;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// State file format version.
pub const STATE_VERSION: u32 = 1;

/// A process that holds a write slot (or a rate-limit refresh claim).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Holder {
    /// Process ID.
    pub pid: u32,
    /// Process start time in clock ticks since boot (`/proc/<pid>/stat` field 22), so a reused
    /// PID is not mistaken for the original holder.
    pub start_ticks: u64,
    /// Random identifier of this gh-paced invocation.
    pub nonce: String,
    /// Request class of the held slot.
    pub class: Class,
    /// When the slot was taken (Unix seconds).
    pub since: f64,
    /// Lease file name (in the state directory) whose lock tracks the slot's real lifetime.
    /// `None` for a refresh claim and for state written by an older version: those fall back to
    /// the PID and start time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
}

/// A host-wide pause after GitHub pushed back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cooldown {
    /// No paced call may start before this time (Unix seconds).
    pub until: f64,
    /// When the pushback was seen.
    pub set_at: f64,
    /// What gh printed that triggered it (pattern names, never the full output).
    pub reason: String,
    /// The command that received the pushback.
    pub command: String,
}

/// Everything gh-paced remembers for one account on one host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct State {
    /// File format version.
    pub version: u32,
    /// Token buckets and hourly windows, keyed by class name.
    pub buckets: BTreeMap<String, Bucket>,
    /// Running WRITE invocations.
    #[serde(default)]
    pub in_flight: Vec<Holder>,
    /// Active pushback cooldown.
    #[serde(default)]
    pub cooldown: Option<Cooldown>,
    /// Last `GET /rate_limit` result.
    #[serde(default)]
    pub rate_limit: Option<Snapshot>,
    /// Paced calls admitted since the last successful rate-limit refresh.
    #[serde(default)]
    pub calls_since_refresh: u32,
    /// Last time any process started a rate-limit refresh (Unix seconds).
    #[serde(default)]
    pub last_refresh_attempt: f64,
    /// The process currently refreshing the rate-limit snapshot.
    #[serde(default)]
    pub refresh_claim: Option<Holder>,
}

impl State {
    /// A fresh state with no history (buckets are created full on first use).
    pub fn new() -> Self {
        Self {
            version: STATE_VERSION,
            ..Self::default()
        }
    }

    /// Drop in-flight entries and refresh claims whose process has exited. Returns the dropped
    /// in-flight entries, so the caller can remove their lease files.
    pub fn reap(&mut self, alive: &dyn Fn(&Holder) -> bool) -> Vec<Holder> {
        let (live, dead): (Vec<Holder>, Vec<Holder>) = std::mem::take(&mut self.in_flight)
            .into_iter()
            .partition(|h| alive(h));
        self.in_flight = live;
        if let Some(claim) = &self.refresh_claim {
            if !alive(claim) {
                self.refresh_claim = None;
            }
        }
        dead
    }
}

/// Validate an account name: GitHub login syntax (letters, digits, hyphens; at most 39).
pub fn valid_account(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 39
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

/// Paths of one account's files.
#[derive(Debug, Clone)]
pub struct Paths {
    /// State directory.
    pub dir: PathBuf,
    /// Account name.
    pub account: String,
}

impl Paths {
    /// Paths for `account` under `dir`.
    pub fn new(dir: PathBuf, account: &str) -> Self {
        Self {
            dir,
            account: account.to_string(),
        }
    }

    /// `<account>.json`.
    pub fn state(&self) -> PathBuf {
        self.dir.join(format!("{}.json", self.account))
    }

    /// `<account>.lock`.
    pub fn lock(&self) -> PathBuf {
        self.dir.join(format!("{}.lock", self.account))
    }

    /// `<account>.audit.jsonl`.
    pub fn audit(&self) -> PathBuf {
        self.dir.join(format!("{}.audit.jsonl", self.account))
    }

    /// `<account>.cooldown`: the latest pushback cooldown, kept apart from the state file.
    pub fn cooldown(&self) -> PathBuf {
        self.dir.join(format!("{}.cooldown", self.account))
    }

    /// Lease file name for an invocation nonce.
    pub fn lease_name(&self, nonce: &str) -> String {
        format!("{}.lease-{nonce}", self.account)
    }

    /// True when `name` is a lease file name of this account (`<account>.lease-<16 hex>`), so a
    /// name read from the state file cannot point outside the state directory.
    pub fn is_lease_name(&self, name: &str) -> bool {
        name.strip_prefix(&format!("{}.lease-", self.account))
            .is_some_and(|n| n.len() == 16 && n.bytes().all(|b| b.is_ascii_hexdigit()))
    }
}

/// Resolve the state directory: `GH_PACED_STATE_DIR`, else `$XDG_STATE_HOME/gh-paced`, else
/// `$HOME/.local/state/gh-paced`.
pub fn state_dir(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    if let Some(dir) = env("GH_PACED_STATE_DIR").filter(|v| !v.is_empty()) {
        return absolute(dir, "GH_PACED_STATE_DIR");
    }
    if let Some(dir) = env("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Ok(absolute(dir, "XDG_STATE_HOME")?.join("gh-paced"));
    }
    let home = env("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "HOME is not set; set GH_PACED_STATE_DIR".to_string())?;
    Ok(absolute(home, "HOME")?.join(".local/state/gh-paced"))
}

fn absolute(dir: String, var: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(dir);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!("{var} must be an absolute path"))
    }
}

/// Create the state directory (mode 0700) if needed.
pub fn ensure_dir(dir: &Path) -> Result<(), String> {
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("cannot create state directory {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// An exclusive lock on one account's state. Dropping it releases the lock.
pub struct LockGuard {
    _file: File,
}

/// How long [`lock`] waits for another process to release the account's lock, seconds. A
/// holder keeps the lock only for bookkeeping, which takes milliseconds: gh-paced never holds
/// it while gh runs, while it sleeps, or while it writes to stderr. A wait this long means the
/// holder is stopped or stuck, and failing (exit 70) is better than hanging every gh call
/// behind it. `GH_PACED_LOCK_WAIT` and the `lock_wait_secs` setting override it.
pub const LOCK_WAIT_SECS: f64 = 30.0;

/// Take the account's exclusive lock, waiting at most [`LOCK_WAIT_SECS`] for it.
pub fn lock(paths: &Paths) -> Result<LockGuard, String> {
    lock_within(paths, LOCK_WAIT_SECS)
}

/// Take the account's exclusive lock, waiting at most `wait_secs` for another process to
/// release it, and fail with an error naming the lock file after that.
pub fn lock_within(paths: &Paths, wait_secs: f64) -> Result<LockGuard, String> {
    lock_within_noting(paths, wait_secs).map(|(guard, _)| guard)
}

/// [`lock_within`], also returning whether another process held the lock at the first try, so
/// that this one waited for it (however briefly).
pub fn lock_within_noting(paths: &Paths, wait_secs: f64) -> Result<(LockGuard, bool), String> {
    lock_within_noting_until(paths, wait_secs, &|| f64::INFINITY)
}

/// [`lock_within_noting`], also giving up once `left`, asked after every failed try, returns
/// no more than 0 seconds. A caller whose own deadline was fixed before this function started
/// passes it here, so that time passing in between (a stall, a suspend) still counts against it.
/// `wait_secs` itself is measured on `std::time::Instant`, as [`lock_within`] always measured
/// it; only `left` decides whether time the machine spends suspended counts.
pub fn lock_within_noting_until(
    paths: &Paths,
    wait_secs: f64,
    left: &dyn Fn() -> f64,
) -> Result<(LockGuard, bool), String> {
    ensure_dir(&paths.dir)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(paths.lock())
        .map_err(|e| format!("cannot open lock file {}: {e}", paths.lock().display()))?;
    let wait = if wait_secs.is_finite() {
        wait_secs.max(0.0)
    } else {
        LOCK_WAIT_SECS
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(wait);
    let mut pause = std::time::Duration::from_millis(1);
    let mut contended = false;
    loop {
        // SAFETY: flock on a valid, owned file descriptor.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EWOULDBLOCK) => {
                contended = true;
                let left = deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .as_secs_f64()
                    .min(left());
                if left <= 0.0 {
                    return Err(format!(
                        "{} is still held by another gh-paced process after {wait} s; that \
                         process is stopped or stuck (GH_PACED_LOCK_WAIT sets this bound)",
                        paths.lock().display()
                    ));
                }
                std::thread::sleep(pause.min(std::time::Duration::from_secs_f64(left)));
                pause = (pause * 2).min(std::time::Duration::from_millis(50));
            }
            _ => return Err(format!("cannot lock {}: {err}", paths.lock().display())),
        }
    }
    Ok((LockGuard { _file: file }, contended))
}

/// Result of loading state.
pub struct Loaded {
    /// The state to use.
    pub state: State,
    /// Set when the file was unreadable and was quarantined.
    pub warning: Option<String>,
    /// True when no state file existed.
    pub fresh: bool,
}

/// Load the account's state. Call only while holding [`lock`].
///
/// `recovery` builds the state that replaces an unusable file; every lease file that is still
/// locked is added to it as an in-flight write, and it is saved before this returns. The
/// cooldown record (`<account>.cooldown`) is merged in: the later of its pause and the state
/// file's wins. An unusable cooldown record is moved aside and the recovery state's blocks and
/// pause are merged in (and saved) instead, because the pause it held is unknown.
pub fn load(paths: &Paths, now: f64, recovery: &dyn Fn(f64) -> State) -> Loaded {
    let mut loaded = load_state_file(paths, now, recovery);
    match read_cooldown(paths) {
        Ok(Some(cd)) => merge_cooldown(&mut loaded.state, cd),
        Ok(None) => {}
        Err(why) => {
            merge_recovery(&mut loaded.state, recovery(now));
            let saved = save(paths, &loaded.state);
            let aside = paths.dir.join(format!(
                "{}.cooldown.corrupt-{}",
                paths.account,
                now.floor() as i64
            ));
            if saved.is_ok() {
                let _ = std::fs::rename(paths.cooldown(), &aside);
            }
            let mut text = format!(
                "cooldown record {} is unusable ({why}); every hourly budget is treated as used \
                 up for the next hour and paced calls pause",
                paths.cooldown().display()
            );
            if let Err(e) = saved {
                text.push_str(&format!("; the recovery state could not be saved: {e}"));
            }
            loaded.warning = Some(match loaded.warning.take() {
                Some(w) => format!("{w}; {text}"),
                None => text,
            });
            loaded.fresh = false;
        }
    }
    loaded
}

fn load_state_file(paths: &Paths, now: f64, recovery: &dyn Fn(f64) -> State) -> Loaded {
    let path = paths.state();
    let text = match std::fs::read(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Loaded {
                state: State::new(),
                warning: None,
                fresh: true,
            }
        }
        Err(e) => {
            return quarantine(paths, now, recovery, &format!("cannot read: {e}"), None);
        }
    };
    match serde_json::from_slice::<State>(&text) {
        Ok(state) if state.version == STATE_VERSION => Loaded {
            state,
            warning: None,
            fresh: false,
        },
        Ok(state) => quarantine(
            paths,
            now,
            recovery,
            &format!("unsupported version {}", state.version),
            Some(&text),
        ),
        Err(e) => quarantine(
            paths,
            now,
            recovery,
            &format!("cannot parse: {e}"),
            Some(&text),
        ),
    }
}

/// The cooldown record, if there is one. Any failure other than "no such file" is an error.
fn read_cooldown(paths: &Paths) -> Result<Option<Cooldown>, String> {
    match std::fs::read(paths.cooldown()) {
        Ok(text) => serde_json::from_slice::<Cooldown>(&text)
            .map(Some)
            .map_err(|e| format!("cannot parse: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read: {e}")),
    }
}

/// Keep the later of `state`'s cooldown and `cd`.
pub fn merge_cooldown(state: &mut State, cd: Cooldown) {
    if state.cooldown.as_ref().is_none_or(|c| c.until < cd.until) {
        state.cooldown = Some(cd);
    }
}

/// Merge a recovery state's per-class blocks and pause into `state`, keeping the later of each.
fn merge_recovery(state: &mut State, recovery: State) {
    for (class, b) in recovery.buckets {
        let entry = state.buckets.entry(class).or_insert_with(|| b.clone());
        if let Some(t) = b.blocked_until {
            if entry.blocked_until.is_none_or(|e| e < t) {
                entry.blocked_until = Some(t);
            }
            entry.level = entry.level.min(b.level);
        }
    }
    if let Some(cd) = recovery.cooldown {
        merge_cooldown(state, cd);
    }
}

/// Lengthen the cooldown record to `cd` unless it already runs at least as long, without
/// touching the state file (the next [`load`] merges the record in). True when it was written.
/// An unreadable record is left for [`load`] to recover from and reported as an error. Call only
/// while holding [`lock`].
pub fn extend_cooldown(paths: &Paths, cd: &Cooldown) -> Result<bool, String> {
    if read_cooldown(paths)?.is_some_and(|old| old.until >= cd.until) {
        return Ok(false);
    }
    save_cooldown(paths, cd).map(|()| true)
}

/// Write the cooldown record atomically. Call only while holding [`lock`].
pub fn save_cooldown(paths: &Paths, cd: &Cooldown) -> Result<(), String> {
    let text = serde_json::to_vec_pretty(cd).map_err(|e| format!("cannot encode cooldown: {e}"))?;
    write_atomically(paths, &paths.cooldown(), "cooldown", &text)
}

/// Load the account's state for display only: nothing is created, moved, quarantined or written,
/// and no lock is needed (saves replace the file atomically). The cooldown record is merged in as
/// [`load`] does. A missing file is a fresh state;
/// an unusable file is an error.
pub fn load_readonly(paths: &Paths) -> Result<State, String> {
    let path = paths.state();
    let mut st = match std::fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        Ok(text) => match serde_json::from_slice::<State>(&text) {
            Ok(state) if state.version == STATE_VERSION => state,
            Ok(state) => {
                return Err(format!(
                    "{} has unsupported version {}; the next paced call will move it aside",
                    path.display(),
                    state.version
                ))
            }
            Err(e) => {
                return Err(format!(
                    "{} is unusable ({e}); the next paced call will move it aside",
                    path.display()
                ))
            }
        },
    };
    match read_cooldown(paths) {
        Ok(Some(cd)) => merge_cooldown(&mut st, cd),
        Ok(None) => {}
        Err(why) => {
            return Err(format!(
                "{} is unusable ({why}); the next paced call will move it aside and pause",
                paths.cooldown().display()
            ))
        }
    }
    Ok(st)
}

/// Keep the unusable state file for inspection and replace it with the recovery state, saved at
/// once. The damaged file is hard-linked (or, failing that, copied) aside rather than renamed,
/// so the state path always holds either it or the saved recovery state: a crash in between
/// makes the next caller recover again, never start fresh.
fn quarantine(
    paths: &Paths,
    now: f64,
    recovery: &dyn Fn(f64) -> State,
    why: &str,
    bytes: Option<&[u8]>,
) -> Loaded {
    let path = paths.state();
    let base = format!("{}.json.corrupt-{}", paths.account, now.floor() as i64);
    let kept = keep_aside(&path, &paths.dir, &base, bytes);
    let mut state = recovery(now);
    let held = held_leases(paths, now);
    let restored = held.len();
    state.in_flight.extend(held);
    let saved = save(paths, &state);
    let where_ = match &kept {
        Some(aside) => format!("a copy is kept at {}", aside.display()),
        None => "no copy could be kept".to_string(),
    };
    let mut warning = format!(
        "state file {} is unusable ({why}); {where_}; every hourly budget is treated as \
         used up for the next hour and paced calls pause, and {restored} running write(s) \
         were restored from their lease files",
        path.display()
    );
    if let Err(e) = saved {
        warning.push_str(&format!("; the recovery state could not be saved: {e}"));
    }
    Loaded {
        state,
        warning: Some(warning),
        fresh: false,
    }
}

/// Hard-link `path` to the first free name among `<base>`, `<base>-1`, `<base>-2`, ... in
/// `dir`, or write `bytes` there when linking fails for another reason. Returns the name used.
fn keep_aside(path: &Path, dir: &Path, base: &str, bytes: Option<&[u8]>) -> Option<PathBuf> {
    for n in 0..100 {
        let aside = if n == 0 {
            dir.join(base)
        } else {
            dir.join(format!("{base}-{n}"))
        };
        match std::fs::hard_link(path, &aside) {
            Ok(()) => return Some(aside),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {}
        }
        let bytes = bytes?;
        let written = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&aside)
            .and_then(|mut f| f.write_all(bytes));
        match written {
            Ok(()) => return Some(aside),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Every lease file of this account that is still locked, as in-flight WRITE holders.
fn held_leases(paths: &Paths, now: f64) -> Vec<Holder> {
    let Ok(entries) = std::fs::read_dir(&paths.dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !paths.is_lease_name(&name) || !lease_held(&paths.dir, &name) {
            continue;
        }
        let nonce = name.rsplit('-').next().unwrap_or_default().to_string();
        out.push(Holder {
            pid: 0,
            start_ticks: 0,
            nonce,
            class: Class::Write,
            since: now,
            lease: Some(name),
        });
    }
    out.sort_by(|a, b| a.nonce.cmp(&b.nonce));
    out
}

/// An open, exclusively locked lease file. Dropping it closes this process's copy; the lock
/// lasts until every process that inherited the descriptor has closed it too.
#[derive(Debug)]
pub struct Lease {
    file: File,
    /// File name in the state directory.
    pub name: String,
}

impl Lease {
    /// The descriptor to keep open in the child.
    pub fn fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }
}

/// Create and lock `<account>.lease-<nonce>`. Call while holding [`lock`].
pub fn create_lease(paths: &Paths, nonce: &str) -> Result<Lease, String> {
    let name = paths.lease_name(nonce);
    let path = paths.dir.join(&name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("cannot create lease {}: {e}", path.display()))?;
    // Blocking: the file is new with a random name, so the only other holder can be a prober's
    // momentary shared lock.
    // SAFETY: flock on a valid, owned file descriptor.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("cannot lock lease {}: {err}", path.display()));
    }
    Ok(Lease { file, name })
}

/// Remove a lease file (after its holder finished or was found dead).
pub fn remove_lease(paths: &Paths, name: &str) {
    if paths.is_lease_name(name) {
        let _ = std::fs::remove_file(paths.dir.join(name));
    }
}

/// True when some process still holds the lock on lease file `name` in `dir`. A missing file is
/// not held. Any error other than "would block" counts as held (fewer calls, not more).
pub fn lease_held(dir: &Path, name: &str) -> bool {
    let file = match File::open(dir.join(name)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    // SAFETY: flock on a valid, owned file descriptor; the probe's lock is released when the
    // file is closed at the end of this function.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if rc == 0 {
        return false;
    }
    let err = std::io::Error::last_os_error();
    err.kind() == std::io::ErrorKind::WouldBlock || err.kind() != std::io::ErrorKind::Interrupted
}

/// Keeps the tests that spawn a child apart from the tests that observe a lock being released.
///
/// Tests run as threads of one process. A spawned child holds a copy of every descriptor in the
/// process, close-on-exec ones included, from the fork until its exec closes them, and the kernel
/// can let the parent go on a moment before that. So a lock that one test has released can still
/// read as held while another test's child is starting, and a sweep or a [`Drop`] that probes it
/// then keeps a directory the test expects to be gone. A test that spawns a child holds this guard
/// from before the spawn until the child is reaped. A test that releases a lock and then observes
/// the release, directly or through code that probes it, holds the guard for its whole body.
#[cfg(test)]
pub(crate) fn child_guard() -> std::sync::MutexGuard<'static, ()> {
    static CHILDREN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    CHILDREN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Save the account's state atomically. Call only while holding [`lock`].
pub fn save(paths: &Paths, state: &State) -> Result<(), String> {
    let text = serde_json::to_vec_pretty(state).map_err(|e| format!("cannot encode state: {e}"))?;
    write_atomically(paths, &paths.state(), "json", &text)
}

/// Write `text` plus a newline to a temporary file in the state directory and rename it over
/// `path`, so readers see the old or the new content, never a mix.
fn write_atomically(paths: &Paths, path: &Path, kind: &str, text: &[u8]) -> Result<(), String> {
    let tmp = paths.dir.join(format!(
        ".{}.{kind}.tmp-{}",
        paths.account,
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    file.write_all(text)
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    drop(file);
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

/// State letter (field 3) and start time (field 22, clock ticks since boot) of
/// `/proc/<pid>/stat`.
fn process_stat(pid: u32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) may contain spaces and parentheses; fields after it are plain.
    let after = &stat[stat.rfind(')')? + 1..];
    let mut fields = after.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ticks = fields.nth(18)?.parse().ok()?;
    Some((state, ticks))
}

/// Start time (field 22 of `/proc/<pid>/stat`) of a process, in clock ticks since boot.
pub fn process_start_ticks(pid: u32) -> Option<u64> {
    process_stat(pid).map(|(_, ticks)| ticks)
}

/// True when the process recorded in `h` is still running: same PID and start time, and not a
/// zombie (`Z`) or dead (`X`) process. A killed process that its parent has not reaped yet keeps
/// its PID and start time but runs nothing, so it must not keep a refresh claim or slot.
pub fn holder_alive(h: &Holder) -> bool {
    process_alive(h.pid, h.start_ticks)
}

/// True when process `pid` started at `start_ticks` (see [`process_start_ticks`]) is still
/// running and is not a zombie or dead process. A reused PID has a different start time.
pub fn process_alive(pid: u32, start_ticks: u64) -> bool {
    matches!(process_stat(pid), Some((state, ticks))
        if ticks == start_ticks && state != 'Z' && state != 'X')
}

/// Liveness of a holder of `paths`' account: its lease lock when it has a valid lease name,
/// otherwise its PID and start time.
pub fn holder_alive_in(paths: &Paths, h: &Holder) -> bool {
    match &h.lease {
        Some(name) if paths.is_lease_name(name) => lease_held(&paths.dir, name),
        _ => holder_alive(h),
    }
}

/// Device and inode of a file.
pub fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// True when descriptor `fd`'s own open file description holds an exclusive `flock`. Linux
/// prints such a lock in `/proc/self/fdinfo/<fd>` as a `lock:` line naming `FLOCK` and `WRITE`,
/// and prints it only for the open file description that took the lock (and every descriptor
/// duplicated or inherited from it): an independent open of the same file shows no lock line
/// even while another process holds one.
pub fn fd_holds_exclusive_flock(fd: i32) -> bool {
    let Ok(text) = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")) else {
        return false;
    };
    text.lines().any(|l| {
        let mut words = l.split_whitespace();
        words.next() == Some("lock:")
            && l.split_whitespace().any(|w| w == "FLOCK")
            && l.split_whitespace().any(|w| w == "WRITE")
    })
}

/// Device and inode of every regular file this process has open through a descriptor that holds
/// an exclusive `flock` (see [`fd_holds_exclusive_flock`]). A lease file appears here only when
/// this process holds the lease's own locked open file, which it can only have inherited (or
/// been handed) from the wrapper that took the lock; opening the lease file again does not count.
pub fn inherited_locked_file_ids() -> Vec<(u64, u64)> {
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return Vec::new();
    };
    let fds: Vec<i32> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok()))
        .collect();
    locked_file_ids(&fds)
}

/// [`inherited_locked_file_ids`] over the given descriptors.
pub fn locked_file_ids(fds: &[i32]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for &fd in fds {
        // SAFETY: fstat writes into a local stat buffer; a closed descriptor just fails.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } == 0
            && (st.st_mode & libc::S_IFMT) == libc::S_IFREG
            && fd_holds_exclusive_flock(fd)
        {
            out.push((st.st_dev, st.st_ino));
        }
    }
    out
}

/// A random-enough identifier for this invocation (no credential material involved).
pub fn new_nonce(now: f64) -> String {
    let mut seed = [0u8; 8];
    let got = File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut seed))
        .is_ok();
    let random = if got {
        u64::from_le_bytes(seed)
    } else {
        (now.to_bits()) ^ u64::from(std::process::id()).rotate_left(32)
    };
    format!("{:016x}", random)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gh-paced-state-{tag}-{}-{}",
            std::process::id(),
            new_nonce(0.0)
        ));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        dir
    }

    #[test]
    fn account_names() {
        assert!(valid_account("octocat"));
        assert!(valid_account("some-bot-2"));
        assert!(!valid_account(""));
        assert!(!valid_account("-lead"));
        assert!(!valid_account("../etc"));
        assert!(!valid_account("a.b"));
        assert!(!valid_account(&"a".repeat(40)));
    }

    #[test]
    fn save_load_round_trip_and_permissions() {
        let dir = tmpdir("rt");
        let paths = Paths::new(dir.clone(), "acct");
        let _g = lock(&paths).expect("lock");
        let loaded = load(&paths, 10.0, &|_| State::new());
        assert!(loaded.fresh);
        let mut s = loaded.state;
        s.calls_since_refresh = 7;
        save(&paths, &s).expect("save");
        let mode = std::fs::metadata(paths.state())
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let again = load(&paths, 11.0, &|_| State::new());
        assert!(!again.fresh);
        assert_eq!(again.state.calls_since_refresh, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn saturated_recovery(now: f64) -> State {
        let mut s = State::new();
        s.buckets
            .insert("write".to_string(), Bucket::saturated(now));
        s.cooldown = Some(Cooldown {
            until: now + 900.0,
            set_at: now,
            reason: "recovery".into(),
            command: "(state recovery)".into(),
        });
        s
    }

    /// A corrupt file is kept aside and replaced by the caller's recovery state, with every
    /// still-locked lease restored as an in-flight write (a dead lease is not). The recovery
    /// state is on disk before `load` returns: the state path never goes missing, so a crash
    /// right after recovery cannot make the next caller start fresh.
    #[test]
    fn corrupt_state_is_quarantined_conservatively() {
        let dir = tmpdir("corrupt");
        let paths = Paths::new(dir.clone(), "acct");
        let live = create_lease(&paths, "00000000000000aa").expect("lease");
        drop(create_lease(&paths, "00000000000000bb").expect("lease"));
        std::fs::write(dir.join("acct.lease-not-a-nonce"), b"").expect("write");
        std::fs::write(paths.state(), b"{not json").expect("write");
        let loaded = load(&paths, 1234.0, &saturated_recovery);
        let warning = loaded.warning.expect("warning");
        assert!(warning.contains("used up for the next hour"), "{warning}");
        assert!(warning.contains("1 running write(s)"), "{warning}");
        assert_eq!(loaded.state.buckets["write"].level, 0.0);
        assert_eq!(
            loaded.state.buckets["write"].blocked_until,
            Some(1234.0 + crate::budget::HOUR)
        );
        assert_eq!(
            loaded.state.in_flight.len(),
            1,
            "{:?}",
            loaded.state.in_flight
        );
        assert_eq!(
            loaded.state.in_flight[0].lease.as_deref(),
            Some(live.name.as_str())
        );
        assert_eq!(loaded.state.in_flight[0].class, Class::Write);
        assert_eq!(
            std::fs::read(dir.join("acct.json.corrupt-1234")).expect("aside"),
            b"{not json"
        );
        let on_disk = load_readonly(&paths).expect("the recovery state was saved");
        assert_eq!(on_disk, loaded.state);
        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two recoveries in the same second keep both damaged files.
    #[test]
    fn repeated_quarantine_keeps_every_damaged_file() {
        let dir = tmpdir("corrupt2");
        let paths = Paths::new(dir.clone(), "acct");
        std::fs::write(paths.state(), b"first").expect("write");
        let _ = load(&paths, 1234.0, &saturated_recovery);
        std::fs::write(paths.state(), b"second").expect("write");
        let loaded = load(&paths, 1234.5, &saturated_recovery);
        assert!(
            loaded
                .warning
                .expect("warning")
                .contains("acct.json.corrupt-1234-1"),
            "second copy named"
        );
        assert_eq!(
            std::fs::read(dir.join("acct.json.corrupt-1234")).expect("first"),
            b"first"
        );
        assert_eq!(
            std::fs::read(dir.join("acct.json.corrupt-1234-1")).expect("second"),
            b"second"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cooldown record keeps a long pushback pause even when the state file is lost: the
    /// recovery pause (15 min) is shorter than the recorded Retry-After (2 h), and the later one
    /// wins. Status sees the same merged pause.
    #[test]
    fn cooldown_record_survives_a_damaged_state_file() {
        let dir = tmpdir("cdrec");
        let paths = Paths::new(dir.clone(), "acct");
        let cd = Cooldown {
            until: 1000.0 + 7200.0,
            set_at: 1000.0,
            reason: "retry-after 7200".into(),
            command: "api POST x".into(),
        };
        save_cooldown(&paths, &cd).expect("save cooldown");
        std::fs::write(paths.state(), b"{damaged").expect("write");
        let loaded = load(&paths, 1100.0, &saturated_recovery);
        assert_eq!(loaded.state.cooldown.as_ref(), Some(&cd));
        assert_eq!(
            load_readonly(&paths).expect("readonly").cooldown,
            Some(cd.clone())
        );
        // A missing state file (fresh start) also takes the recorded pause.
        std::fs::remove_file(paths.state()).expect("rm");
        let fresh = load(&paths, 1200.0, &saturated_recovery);
        assert!(fresh.fresh);
        assert_eq!(fresh.state.cooldown, Some(cd.clone()));
        assert_eq!(load_readonly(&paths).expect("readonly").cooldown, Some(cd));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unusable cooldown record is moved aside and replaced by the recovery state's blocks
    /// and pause, saved at once; status reports it instead of guessing.
    #[test]
    fn damaged_cooldown_record_recovers_conservatively() {
        let dir = tmpdir("cdbad");
        let paths = Paths::new(dir.clone(), "acct");
        save(&paths, &State::new()).expect("save");
        std::fs::write(paths.cooldown(), b"{half").expect("write");
        assert!(load_readonly(&paths).is_err());
        let loaded = load(&paths, 1234.0, &saturated_recovery);
        let warning = loaded.warning.expect("warning");
        assert!(warning.contains("cooldown record"), "{warning}");
        assert_eq!(
            loaded.state.buckets["write"].blocked_until,
            Some(1234.0 + crate::budget::HOUR)
        );
        assert_eq!(
            loaded.state.cooldown.as_ref().map(|c| c.until),
            Some(2134.0)
        );
        assert!(!paths.cooldown().exists());
        assert!(dir.join("acct.cooldown.corrupt-1234").exists());
        assert_eq!(load_readonly(&paths).expect("saved"), loaded.state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A lease is held while any open file description of it is open: the creating process's,
    /// or a child's that inherited it. Another open of the same file probes it.
    #[test]
    fn lease_lifetime_follows_the_open_file() {
        let _children = child_guard();
        let dir = tmpdir("lease");
        let paths = Paths::new(dir.clone(), "acct");
        let lease = create_lease(&paths, "0123456789abcdef").expect("lease");
        assert_eq!(lease.name, "acct.lease-0123456789abcdef");
        let h = Holder {
            pid: 0,
            start_ticks: 0,
            nonce: "0123456789abcdef".into(),
            class: Class::Write,
            since: 0.0,
            lease: Some(lease.name.clone()),
        };
        assert!(lease_held(&dir, &lease.name));
        assert!(
            holder_alive_in(&paths, &h),
            "pid 0 is ignored when a lease exists"
        );
        // A child that inherits the descriptor keeps the lease after this process lets go.
        // SAFETY: duplicating a descriptor this test owns, without close-on-exec.
        let inherited = unsafe { libc::dup(lease.fd()) };
        assert!(inherited >= 0);
        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .expect("spawn sleep");
        // SAFETY: closing the duplicate in this process; the child keeps its own copy.
        unsafe {
            libc::close(inherited);
        }
        let name = lease.name.clone();
        drop(lease);
        assert!(lease_held(&dir, &name), "the child still holds it");
        child.wait().expect("wait");
        assert!(
            !lease_held(&dir, &name),
            "released when the last holder exits"
        );
        assert!(!holder_alive_in(&paths, &h));
        remove_lease(&paths, &name);
        assert!(!dir.join(&name).exists());
        assert!(!lease_held(&dir, &name), "a missing lease is not held");
        // A name that is not this account's lease falls back to PID liveness.
        let forged = Holder {
            lease: Some("../../etc/passwd".into()),
            ..h
        };
        assert!(!holder_alive_in(&paths, &forged));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only a descriptor sharing the lease's locked open file proves descent. An independent
    /// open of the same lease file (the spoof: stdin redirected from it) does not, even while the
    /// lease is held; a duplicate of the locked descriptor (what a child inherits) does.
    #[test]
    fn only_the_locked_open_file_proves_descent() {
        let dir = tmpdir("ids");
        let paths = Paths::new(dir.clone(), "acct");
        let lease = create_lease(&paths, "00000000000000cc").expect("lease");
        let id = file_id(&dir.join(&lease.name)).expect("id");
        let independent = File::open(dir.join(&lease.name)).expect("open");
        assert!(fd_holds_exclusive_flock(lease.fd()));
        assert!(!fd_holds_exclusive_flock(independent.as_raw_fd()));
        assert!(locked_file_ids(&[independent.as_raw_fd()]).is_empty());
        assert_eq!(locked_file_ids(&[lease.fd()]), vec![id]);
        // SAFETY: duplicating a descriptor this test owns; closed below.
        let dup = unsafe { libc::dup(lease.fd()) };
        assert!(dup >= 0);
        assert_eq!(locked_file_ids(&[dup]), vec![id]);
        assert!(inherited_locked_file_ids().contains(&id));
        // SAFETY: closing the duplicate made above.
        unsafe {
            libc::close(dup);
        }
        // A plain open regular file that is not locked is not listed.
        let plain = dir.join("plain");
        std::fs::write(&plain, b"x").expect("write");
        let f = File::open(&plain).expect("open");
        assert!(!inherited_locked_file_ids().contains(&file_id(&plain).expect("id")));
        drop(f);
        drop(independent);
        drop(lease);
        assert!(!inherited_locked_file_ids().contains(&id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A killed process that has not been reaped (a zombie) keeps its PID and start time but is
    /// not alive, so it cannot keep a refresh claim; reaping drops it.
    #[test]
    fn zombies_are_not_alive() {
        let _children = child_guard();
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        let ticks = process_start_ticks(pid).expect("child stat");
        let begun = std::time::Instant::now();
        while process_stat(pid).map(|(s, _)| s) != Some('Z') {
            assert!(
                begun.elapsed().as_secs() < 10,
                "child never became a zombie"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let claim = Holder {
            pid,
            start_ticks: ticks,
            nonce: "z".into(),
            class: Class::Read,
            since: 0.0,
            lease: None,
        };
        assert_eq!(
            process_start_ticks(pid),
            Some(ticks),
            "PID and start time unchanged"
        );
        assert!(!holder_alive(&claim));
        let mut s = State::new();
        s.refresh_claim = Some(claim);
        s.reap(&holder_alive);
        assert!(s.refresh_claim.is_none(), "the claim is recovered");
        child.wait().expect("reap");
    }

    #[test]
    fn liveness_uses_start_time() {
        let me = std::process::id();
        let ticks = process_start_ticks(me).expect("own stat");
        let h = Holder {
            pid: me,
            start_ticks: ticks,
            nonce: "n".into(),
            class: Class::Write,
            since: 0.0,
            lease: None,
        };
        assert!(holder_alive(&h));
        let stale = Holder {
            start_ticks: ticks + 1,
            ..h.clone()
        };
        assert!(!holder_alive(&stale));
        let mut s = State::new();
        s.in_flight = vec![h.clone(), stale.clone()];
        assert_eq!(s.reap(&holder_alive), vec![stale]);
        assert_eq!(s.in_flight, vec![h]);
    }

    #[test]
    fn state_dir_resolution() {
        let env = |k: &str| match k {
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        };
        assert_eq!(
            state_dir(&env).expect("dir"),
            PathBuf::from("/home/u/.local/state/gh-paced")
        );
        let env = |k: &str| match k {
            "XDG_STATE_HOME" => Some("/x".to_string()),
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        };
        assert_eq!(state_dir(&env).expect("dir"), PathBuf::from("/x/gh-paced"));
        let env = |k: &str| match k {
            "GH_PACED_STATE_DIR" => Some("relative".to_string()),
            _ => None,
        };
        assert!(state_dir(&env).is_err());
    }
}
