//! Evidence that survives the kill: durable per-step logs and test-level culprit attribution.
//!
//! THE PROBLEM THIS SOLVES. Two facts about the runner used to combine badly. Captured step output
//! lived only in memory until the step ended, and the per-step profile rows are written at END OF
//! RUN. So the one mechanism that reliably stops a runaway — a hard kill, whether ours or the CI
//! provider's job cancellation — is also the mechanism that destroys every trace of why it was
//! needed. A cancelled GitHub job discards its logs entirely. That is the single largest reason a
//! slow-step regression stayed unexplained for days: the lane could not report on its own failure.
//!
//! The fix is INCREMENTAL EMISSION. Everything here is appended to disk and flushed AS IT HAPPENS,
//! never accumulated for an end-of-run summary:
//!
//! * a per-step `<tag>.log` receiving the step's interleaved stdout/stderr byte-for-byte, and
//! * a run-level `journal.jsonl` of one-line records: step start, each recognized test boundary,
//!   teardown, and the culprit verdict.
//!
//! When the operator configures an evidence directory, kill the runner with SIGKILL at any instant
//! and that directory still describes what was running.
//!
//! TEST-LEVEL ATTRIBUTION. Naming the NODE that hung ("the strict-compat step") is not enough to
//! act on; the actionable fact is WHICH TEST inside that node. [`TestTracker`] derives it from the
//! step's own output stream as the bytes arrive, tracking every test STARTED against its matching
//! completion. On teardown the complete concurrent live set and elapsed time for each survives;
//! the longest-running one is only a likely culprit when several remain.
//!
//! THE UNTERMINATED TAIL IS THE STRONGEST SIGNAL, and it is why the reader here splits lines
//! itself instead of using `read_until(b'\n')`. Rust's libtest prints `test some::name ... ` and
//! then RUNS the test, emitting the verdict on the same line afterwards. A line-oriented reader
//! sees that text only once the test finishes — precisely never, for the test that hung. Reading
//! chunks and keeping the bytes since the last newline leaves the hung test's own start marker
//! sitting in [`StepStream::partial`] at the moment of the kill.
//!
//! WHAT IS RECOGNIZED. Rust libtest, pytest (`-v`), TAP, and an explicit `##TEST-START`/`##TEST-END`
//! protocol any harness can adopt. A pre-signal `/proc` snapshot separately distinguishes CPU-
//! burning from wall-stalled descendants and recognizes nextest's exact libtest argv binding. An
//! unrecognized/shared-process harness degrades honestly: the node and processes are named but no
//! test is, which is strictly better than a guess dressed as attribution.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Environment variable that overrides the evidence directory.
pub const LOG_DIR_ENV: &str = "DAGRUN_LOG_DIR";
/// Set to `1` to disable durable per-step logs and the journal entirely.
pub const NO_LOGS_ENV: &str = "DAGRUN_NO_STEP_LOGS";
/// Per-step ceiling, in bytes, on the durable raw-output log. `0` means unlimited.
pub const LOG_MAX_BYTES_ENV: &str = "DAGRUN_LOG_MAX_BYTES";
/// Default per-step durable-log ceiling: 1 GiB.
///
/// WHY A CEILING AT ALL. A step log is a byte-for-byte copy of the step's raw output --
/// measured, not estimated: a step emitting exactly 100 MiB produced a 104,857,600-byte log.
/// So a step that runs away on its output stream does not merely go unbounded here, it is
/// DUPLICATED onto the filesystem: once by whatever the step itself writes, and again by this
/// capture. A multi-terabyte runaway therefore costs twice its own size, on the same device the
/// build is running from, at exactly the moment that device can least afford it.
///
/// WHY 1 GiB. Large enough that no honest step is truncated, and small enough that a runaway is
/// stopped while the filesystem still has room to be useful.
pub const DEFAULT_LOG_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// Exact bytes appended once when a step log hits its ceiling.
///
/// A reader of a capped log must be able to tell "the step printed nothing more" from "we
/// stopped writing", so the log says which one it is, in band, exactly once.
// MAINTENANCE, deliberately not rustdoc: this crate ships a second engine whose logs are
// compared against this one byte-for-byte by the differential harness. Change this string in
// one engine only and that comparison fails. Keep both in step.
pub const TRUNCATION_MARKER: &str = "\n[dagrun] STEP LOG TRUNCATED at this ceiling \
     (raise or lift it with DAGRUN_LOG_MAX_BYTES; 0 = unlimited). \
     Test classification and attribution CONTINUE; only durable capture stopped.\n";

/// The configured per-step log ceiling in bytes, or `None` for unlimited.
///
/// An unparseable or negative value is a configuration error the operator must see, so it is
/// reported and then treated as the default rather than silently ignored -- a misread ceiling
/// that quietly becomes "unlimited" is the failure this whole ceiling exists to prevent.
pub fn log_max_bytes() -> Option<u64> {
    match std::env::var(LOG_MAX_BYTES_ENV) {
        Err(_) => Some(DEFAULT_LOG_MAX_BYTES),
        Ok(raw) if raw.trim().is_empty() => Some(DEFAULT_LOG_MAX_BYTES),
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => {
                eprintln!(
                    "[dagrun] WARNING: {LOG_MAX_BYTES_ENV}={raw:?} is not a \
                     non-negative integer; using the {DEFAULT_LOG_MAX_BYTES}-byte default."
                );
                Some(DEFAULT_LOG_MAX_BYTES)
            }
        },
    }
}

/// Environment variable overriding the per-step IN-MEMORY capture ceiling (0 = unlimited).
pub const CAPTURE_MAX_BYTES_ENV: &str = "DAGRUN_CAPTURE_MAX_BYTES";

/// Default per-step in-memory capture ceiling: 4 MiB of the step's output TAIL.
///
/// WHY A SECOND CEILING. [`DEFAULT_LOG_MAX_BYTES`] bounds the copy on DISK. It says nothing about
/// the copy the runner holds in its OWN address space for the whole life of the step, and that is
/// the copy that kills the runner first: a step whose output runs away is held entirely in RSS,
/// and the failure path then joins it into one contiguous buffer, doubling the peak at exactly
/// the worst moment. A multi-terabyte runaway OOMs the runner -- taking the run's verdict, its
/// profile rows and its evidence with it -- long before it fills the device the disk ceiling
/// protects.
///
/// WHY A TAIL, AND WHY THIS SIZE. Everything the in-memory capture feeds is a tail: the one-line
/// summary and the failure dump a human actually reads. 4 MiB is far more tail than anyone reads
/// and still bounds the whole-step peak at twice that plus a constant. The durable log is
/// unaffected and remains the place to go for the complete output.
pub const DEFAULT_CAPTURE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Exact notice prepended to a failure dump whose in-memory capture dropped earlier output.
// MAINTENANCE, deliberately not rustdoc: this crate ships a second engine whose output is
// compared against this one byte-for-byte by the differential harness. Keep both in step.
pub fn capture_truncation_notice(total: u64, kept: usize) -> String {
    format!(
        "[dagrun] EARLIER OUTPUT DROPPED: this step produced {total} bytes but only \
         the last {kept} were kept in memory (raise or lift the ceiling with \
         DAGRUN_CAPTURE_MAX_BYTES; 0 = unlimited). The durable per-step log is \
         unaffected and still has the rest."
    )
}

/// The configured per-step in-memory capture ceiling in bytes, or `None` for unlimited.
///
/// Same posture as [`log_max_bytes`]: an unparseable value is REPORTED and then treated as the
/// default, because a misread ceiling that quietly becomes "unlimited" is precisely the failure
/// the ceiling exists to prevent.
pub fn capture_max_bytes() -> Option<usize> {
    match std::env::var(CAPTURE_MAX_BYTES_ENV) {
        Err(_) => Some(DEFAULT_CAPTURE_MAX_BYTES),
        Ok(raw) if raw.trim().is_empty() => Some(DEFAULT_CAPTURE_MAX_BYTES),
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => {
                eprintln!(
                    "[dagrun] WARNING: {CAPTURE_MAX_BYTES_ENV}={raw:?} is not a \
                     non-negative integer; using the {DEFAULT_CAPTURE_MAX_BYTES}-byte default."
                );
                Some(DEFAULT_CAPTURE_MAX_BYTES)
            }
        },
    }
}

/// Environment variable carrying the per-step ownership nonce (see `scheduler::kill_by_nonce`).
pub const STEP_NONCE_ENV: &str = "DAGRUN_STEP";
/// Path where a controlled test runner may write its structured test counts.
///
/// The scheduler owns the path and installs it after the manifest-provided
/// environment, so a DAG cannot redirect the evidence to a stale or shared
/// file. The file format is documented in the crate README.
pub const TEST_COUNTS_PATH_ENV: &str = "DAGRUN_TEST_COUNTS_PATH";
const MAX_COMPONENT_BYTES: usize = 255;

/// Monotonic per-process counter making each step's nonce unique within this runner.
static NONCE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Mint a per-step ownership token that every descendant inherits through its environment.
///
/// The token is the runner's pid, a process-start-relative sequence number, and the wall clock in
/// nanoseconds, so it is unique across concurrent steps, across runs, and across pid reuse.
pub fn mint_step_nonce(tag: &str) -> String {
    let seq = NONCE_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // The tag is included for human readability when the token shows up in a `ps` listing; the
    // uniqueness comes entirely from pid/seq/nanos, never from the tag.
    format!("{}:{}:{}:{}", std::process::id(), seq, nanos, sanitize(tag))
}

/// Filesystem-safe rendering of a step tag (used for nonces and log file names).
pub fn sanitize(tag: &str) -> String {
    let mut out = String::with_capacity(tag.len() * 3);
    for byte in tag.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-') {
            out.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "~{byte:02x}");
        }
    }
    out
}

/// Where a run's durable evidence is written, or `None` when the operator opted out.
///
/// Evidence is opt-in so the package does not create an unbounded, world-discoverable accumulation
/// under a shared `/tmp`. An explicitly configured directory is made private and validated by
/// [`RunEvidence::open`].
pub fn default_log_dir() -> Option<PathBuf> {
    if std::env::var(NO_LOGS_ENV)
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return None;
    }
    std::env::var(LOG_DIR_ENV)
        .ok()
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

/// Seconds since the epoch, as a fixed-precision string for journal records.
fn now_s() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", d.as_secs(), d.subsec_millis())
}

/// Minimal JSON string escaping (the journal writes only strings and numbers).
fn jesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The run-level append-only journal: one JSON object per line, flushed on every write.
///
/// Flushing every record is the whole point. A buffered writer would hold the last — and therefore
/// most interesting — records in memory at exactly the moment a SIGKILL discards them.
pub struct RunEvidence {
    dir: PathBuf,
    journal: Mutex<Option<File>>,
}

/// A typed field in the JSONL evidence stream.
pub(crate) enum JournalValue {
    Text(String),
    Boolean(bool),
}

/// Return a `step_end` verdict, refusing absent or non-boolean values.
pub fn require_step_end_ok(record: &serde_json::Value) -> Result<bool, String> {
    if record.get("event").and_then(serde_json::Value::as_str) != Some("step_end") {
        return Err("journal record is not a step_end event".to_string());
    }
    record
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "journal step_end ok must be a boolean".to_string())
}

impl RunEvidence {
    /// Create the evidence directory and open the journal. Returns `None` when logging is disabled
    /// or the directory cannot be created; evidence capture must never be able to fail a run.
    pub fn open(dir: Option<PathBuf>) -> Option<Self> {
        let dir = dir?;
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);
        if builder.create(&dir).is_err() {
            eprintln!(
                "[scheduler] WARNING: could not create evidence directory {}; per-step logs and \
                 test-level attribution are disabled for this run.",
                dir.display()
            );
            return None;
        }
        let metadata = match std::fs::symlink_metadata(&dir) {
            Ok(metadata) => metadata,
            Err(error) => {
                eprintln!(
                    "[scheduler] WARNING: could not inspect evidence directory {} ({error}); \
                     per-step logs and attribution are disabled for this run.",
                    dir.display()
                );
                return None;
            }
        };
        // Never chmod a pre-existing caller path: refuse it if its ownership or privacy is wrong.
        if !metadata.file_type().is_dir()
            || metadata.uid() != effective_uid()
            || metadata.permissions().mode() & 0o700 != 0o700
            || metadata.permissions().mode() & 0o077 != 0
        {
            eprintln!(
                "[scheduler] WARNING: evidence directory {} is not a private, owned directory; \
                 per-step logs and attribution are disabled for this run.",
                dir.display()
            );
            return None;
        }
        let journal = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(dir.join("journal.jsonl"))
            .ok()
            .filter(private_regular_file);
        let Some(journal) = journal else {
            eprintln!(
                "[scheduler] WARNING: evidence journal {} is not a private, owned regular file; \
                 per-step logs and attribution are disabled for this run.",
                dir.join("journal.jsonl").display()
            );
            return None;
        };
        Some(RunEvidence {
            dir,
            journal: Mutex::new(Some(journal)),
        })
    }

    /// The directory holding this run's per-step logs and journal.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append one record and flush it. Fields are emitted in the given order after `ts`/`event`.
    pub fn record(&self, event: &str, fields: &[(&str, String)]) {
        let fields: Vec<(&str, JournalValue)> = fields
            .iter()
            .map(|(key, value)| (*key, JournalValue::Text(value.clone())))
            .collect();
        self.record_values(event, &fields);
    }

    /// Append one record with explicit JSON value types and flush it.
    pub(crate) fn record_values(&self, event: &str, fields: &[(&str, JournalValue)]) {
        let mut line = format!("{{\"ts\":\"{}\",\"event\":\"{}\"", now_s(), jesc(event));
        for (k, v) in fields {
            match v {
                JournalValue::Text(value) => {
                    line.push_str(&format!(",\"{}\":\"{}\"", jesc(k), jesc(value)));
                }
                JournalValue::Boolean(value) => {
                    line.push_str(&format!(",\"{}\":{}", jesc(k), value));
                }
            }
        }
        line.push_str("}\n");
        if let Ok(mut guard) = self.journal.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
        }
    }

    /// Open (create/truncate) the durable log file for one step.
    pub fn open_step_log(&self, tag: &str) -> Option<File> {
        let name = format!("{}.log", sanitize(tag));
        if name.len() > MAX_COMPONENT_BYTES {
            return None;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.dir.join(name))
            .ok()
            .filter(private_regular_file)?;
        // Validate before truncating: `truncate(true)` in the open itself would destroy the target
        // of a planted hard link before `fstat` had a chance to reject it.
        file.set_len(0).ok()?;
        Some(file)
    }
}

fn private_regular_file(file: &File) -> bool {
    file.metadata()
        .map(|metadata| {
            metadata.file_type().is_file()
                && metadata.uid() == effective_uid()
                && metadata.permissions().mode() & 0o077 == 0
                && metadata.nlink() == 1
        })
        .unwrap_or(false)
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments and reads process credentials only.
    unsafe { libc::geteuid() }
}

/// A recognized test-boundary event in a step's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestEvent {
    /// A named test began running.
    Start(String),
    /// A named test finished, with the harness's verdict token (`ok`, `FAILED`, `PASSED`, ...).
    End(String, String),
}

/// Recognize a test boundary in one line of harness output.
///
/// DELIBERATELY CONSERVATIVE. A false positive renames the culprit and is worse than no name at
/// all, so each pattern is anchored at the start of the line and requires the harness's own
/// punctuation. Anything unrecognized returns `None` and the step simply has no test-level detail.
pub fn recognize(line: &str) -> Option<TestEvent> {
    // NOTE the asymmetry: TRAILING whitespace is preserved for the libtest check below, because
    // `test name ... ` with nothing after the ellipsis is exactly the announcement of a test that
    // has begun but not finished — trimming it away would erase the distinction between "started"
    // and "finished" and with it every hung-test attribution.
    let raw = line.trim_start().trim_end_matches(['\n', '\r']);
    let s = raw.trim_end();

    // 1) Explicit protocol, for any harness that wants exact attribution:
    //      ##TEST-START <name>   ##TEST-END <name> [verdict]
    if let Some(rest) = s.strip_prefix("##TEST-START ") {
        let name = rest.trim();
        if !name.is_empty() {
            return Some(TestEvent::Start(name.to_string()));
        }
    }
    if let Some(rest) = s.strip_prefix("##TEST-END ") {
        let mut it = rest.trim().splitn(2, char::is_whitespace);
        let name = it.next().unwrap_or("").trim();
        if !name.is_empty() {
            let verdict = it.next().unwrap_or("end").trim();
            return Some(TestEvent::End(name.to_string(), verdict.to_string()));
        }
    }

    // 2) Rust libtest: "test some::name ... ok" (END) or a bare "test some::name ... " (START).
    //    Captured mode emits the whole line at once, so the same text yields END; --nocapture
    //    flushes the prefix before running the test, which is what makes START reachable at all.
    if let Some(rest) = raw.strip_prefix("test ") {
        // `" ... "` (with a verdict or not) and a bare `" ..."` at end of buffer are the two forms.
        let split = rest
            .split_once(" ... ")
            .or_else(|| rest.strip_suffix(" ...").map(|n| (n, "")));
        if let Some((name, after)) = split {
            let name = name.trim();
            if !name.is_empty() && !name.contains(' ') {
                let verdict = after.trim();
                return Some(if verdict.is_empty() {
                    TestEvent::Start(name.to_string())
                } else {
                    TestEvent::End(name.to_string(), first_token(verdict))
                });
            }
        }
    }

    // 3) pytest -v / -rA: "path/to/test_x.py::test_name PASSED" (also FAILED/ERROR/SKIPPED/XFAIL).
    if let Some((head, tail)) = s.rsplit_once(char::is_whitespace) {
        let verdict = first_token(tail);
        if head.contains("::")
            && !head.contains(' ')
            && matches!(
                verdict.as_str(),
                "PASSED" | "FAILED" | "ERROR" | "SKIPPED" | "XFAIL" | "XPASS"
            )
        {
            return Some(TestEvent::End(head.to_string(), verdict));
        }
    }
    // A pytest nodeid alone on the line (verbose + unbuffered) marks the START of that test.
    if s.contains("::") && !s.contains(' ') && s.len() > 2 {
        return Some(TestEvent::Start(s.to_string()));
    }

    // 4) TAP: "ok 12 - name" / "not ok 12 - name".
    for (prefix, verdict) in [("not ok ", "not ok"), ("ok ", "ok")] {
        if let Some(rest) = s.strip_prefix(prefix) {
            if let Some((num, name)) = rest.split_once(" - ") {
                if num.trim().parse::<u64>().is_ok() && !name.trim().is_empty() {
                    return Some(TestEvent::End(name.trim().to_string(), verdict.to_string()));
                }
            }
        }
    }

    None
}

fn first_token(s: &str) -> String {
    s.split_whitespace().next().unwrap_or("").to_string()
}

/// The last test seen to start, the last seen to finish, and how many finished.
#[derive(Debug, Default, Clone)]
pub struct TestTracker {
    /// The most recent test seen to BEGIN. On a hang this is the prime suspect.
    pub last_started: Option<String>,
    /// The most recent test seen to FINISH; it bounds the culprit even when none is named.
    pub last_completed: Option<String>,
    /// How many tests finished (the denominator for "how far into the suite did it get").
    pub completed: u64,
    /// How many tests began.
    pub started: u64,
    /// Every test announced as started and awaiting its end event, with a monotonic start instant.
    ///
    /// A parallel harness can have several of these at once. Keeping the set is the difference
    /// between reporting the last line we happened to read and reporting the work actually live
    /// when teardown began.
    in_flight: BTreeMap<String, Instant>,
}

impl TestTracker {
    fn observe(&mut self, ev: &TestEvent) {
        match ev {
            TestEvent::Start(name) => {
                self.last_started = Some(name.clone());
                if !self.in_flight.contains_key(name) {
                    self.started += 1;
                    self.in_flight.insert(name.clone(), Instant::now());
                }
            }
            TestEvent::End(name, _) => {
                // A captured-mode libtest line is both the start and the end of that test.
                if self.last_started.as_deref() != Some(name.as_str()) {
                    self.last_started = Some(name.clone());
                    self.started += 1;
                }
                self.in_flight.remove(name);
                self.last_completed = Some(name.clone());
                self.completed += 1;
            }
        }
    }

    fn in_flight_snapshot(&self) -> Vec<InFlightTest> {
        let now = Instant::now();
        let mut tests = self
            .in_flight
            .iter()
            .map(|(name, started)| InFlightTest {
                name: name.clone(),
                elapsed_s: now.saturating_duration_since(*started).as_secs_f64(),
                basis: "declared test boundary",
            })
            .collect::<Vec<_>>();
        tests.sort_by(|a, b| {
            b.elapsed_s
                .partial_cmp(&a.elapsed_s)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.cmp(&b.name))
        });
        tests
    }
}

/// One test known to be live at the termination boundary.
#[derive(Debug, Clone)]
pub struct InFlightTest {
    /// Harness-level test identifier.
    pub name: String,
    /// Seconds elapsed from the observed start boundary to this snapshot.
    pub elapsed_s: f64,
    /// Observable fact binding the name to the running work.
    pub basis: &'static str,
}

/// The runner's verdict about which test was responsible when a step had to be terminated.
#[derive(Debug, Clone)]
pub struct Culprit {
    /// The test's name, when one could be attributed.
    pub test: Option<String>,
    /// How the name was derived — quoted verbatim in reports so a reader can weigh it.
    pub how: &'static str,
    /// Tests that completed before the kill (the denominator for "how far did it get").
    pub completed: u64,
    /// The last test that DID complete, which bounds the culprit even when it is unnamed.
    pub last_completed: Option<String>,
    /// Complete current snapshot. The first entry is the longest-running and therefore the
    /// likely culprit when several tests are live; the list prevents that heuristic from hiding
    /// its denominator.
    pub in_flight: Vec<InFlightTest>,
}

impl Culprit {
    /// One-line human rendering for the step's outcome line.
    pub fn describe(&self) -> String {
        let live = if self.in_flight.is_empty() {
            String::new()
        } else {
            format!(
                "; {} test(s) in flight [{}]",
                self.in_flight.len(),
                self.in_flight
                    .iter()
                    .map(|t| format!("{} {:.3}s via {}", t.name, t.elapsed_s, t.basis))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        match &self.test {
            Some(name) => format!(
                "{}culprit test {name} ({}; {} test(s) completed first{}{})",
                if self.in_flight.len() > 1 {
                    "likely "
                } else {
                    ""
                },
                self.how,
                self.completed,
                match &self.last_completed {
                    Some(l) => format!(", last completed {l}"),
                    None => String::new(),
                },
                live,
            ),
            None => format!(
                "no test-level attribution ({}; {} test(s) completed{}{})",
                self.how,
                self.completed,
                match &self.last_completed {
                    Some(l) => format!(", last completed {l}"),
                    None => String::new(),
                },
                live,
            ),
        }
    }
}

/// One process observed in the owned step tree immediately before graceful termination.
///
/// This is deliberately evidence, not a test verdict. A third-party runner may not expose a
/// process-to-test binding at all; in that case `test` stays `None` rather than guessing from a
/// binary name. CPU-burning and wall-stalled are separate signatures because they point at
/// different failure modes.
#[derive(Debug, Clone)]
pub struct ProcessObservation {
    /// Linux process identifier observed in `/proc`.
    pub pid: u32,
    /// Linux parent process identifier from the same snapshot.
    pub ppid: u32,
    /// Bounded command-line rendering, or a PID fallback when unavailable.
    pub command: String,
    /// Process lifetime at the snapshot boundary.
    pub wall_elapsed_s: f64,
    /// User plus system CPU consumed over that lifetime.
    pub cpu_elapsed_s: f64,
    /// `cpu-burning`, `wall-stalled`, or `mixed-or-too-young`.
    pub signature: &'static str,
    /// Test identifier only when process argv carries an exact binding.
    pub test: Option<String>,
    /// Observable mechanism supporting `test`.
    pub test_basis: Option<&'static str>,
}

#[derive(Debug)]
struct ProcRow {
    pid: u32,
    ppid: u32,
    state: char,
    utime_ticks: u64,
    stime_ticks: u64,
    start_ticks: u64,
    argv: Vec<String>,
    carries_nonce: bool,
}

fn boot_elapsed_s() -> Option<f64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes one initialized timespec through a valid pointer.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 || ts.tv_sec < 0 {
        return None;
    }
    Some(ts.tv_sec as f64 + ts.tv_nsec as f64 / 1_000_000_000.0)
}

fn proc_row(pid: u32, nonce: Option<&str>) -> Option<ProcRow> {
    let stat = crate::procstat::read(pid).ok().flatten()?;
    let state = char::from(stat.state);
    let ppid = stat.ppid;
    let utime_ticks = stat.utime_ticks;
    let stime_ticks = stat.stime_ticks;
    let start_ticks = stat.starttime_ticks;
    let argv = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|raw| {
            raw.split(|b| *b == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let carries_nonce = nonce
        .filter(|value| !value.is_empty())
        .and_then(|value| {
            let needle = format!("{STEP_NONCE_ENV}={value}");
            std::fs::read(format!("/proc/{pid}/environ"))
                .ok()
                .map(|raw| raw.split(|b| *b == 0).any(|item| item == needle.as_bytes()))
        })
        .unwrap_or(false);
    Some(ProcRow {
        pid,
        ppid,
        state,
        utime_ticks,
        stime_ticks,
        start_ticks,
        argv,
        carries_nonce,
    })
}

fn exact_libtest_from_argv(argv: &[String]) -> Option<String> {
    let exact = argv.iter().position(|arg| arg == "--exact")?;
    // libtest accepts both `--exact TEST` and `TEST --exact`; nextest currently uses the former.
    for candidate in [
        argv.get(exact + 1),
        exact.checked_sub(1).and_then(|i| argv.get(i)),
    ]
    .into_iter()
    .flatten()
    {
        if !candidate.is_empty() && !candidate.starts_with('-') && candidate != &argv[0] {
            return Some(candidate.clone());
        }
    }
    None
}

fn process_signature(state: char, wall_elapsed_s: f64, cpu_elapsed_s: f64) -> &'static str {
    let cpu_ratio = if wall_elapsed_s > 0.0 {
        cpu_elapsed_s / wall_elapsed_s
    } else {
        0.0
    };
    // A runnable process is direct evidence of current CPU demand. Preserve that evidence under
    // host contention, where an actual busy loop can receive less than half of elapsed wall time.
    if cpu_elapsed_s >= 0.25 && (cpu_ratio >= 0.50 || state == 'R') {
        "cpu-burning"
    } else if wall_elapsed_s >= 0.50 && cpu_ratio <= 0.05 {
        "wall-stalled"
    } else {
        "mixed-or-too-young"
    }
}

/// Snapshot every process observably owned by a step, before sending it SIGTERM.
///
/// Ownership is root-descendant reachability plus the runner-minted exact environment nonce. The
/// nonce includes setsid/double-fork escapees without falling back to a process-name match. Rows
/// are stable-sorted by PID only for reproducible evidence; no ordering is interpreted as blame.
pub fn process_snapshot(root: u32, nonce: Option<&str>) -> Vec<ProcessObservation> {
    if root <= 1 {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut rows = HashMap::new();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if let Some(row) = proc_row(pid, nonce) {
            rows.insert(pid, row);
        }
    }
    let mut owned = HashSet::from([root]);
    loop {
        let before = owned.len();
        for row in rows.values() {
            if owned.contains(&row.ppid) || row.carries_nonce {
                owned.insert(row.pid);
            }
        }
        if owned.len() == before {
            break;
        }
    }
    let Some(boot_s) = boot_elapsed_s() else {
        return Vec::new();
    };
    // SAFETY: sysconf reads one process-global constant and dereferences no pointers.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks <= 0 {
        return Vec::new();
    }
    let ticks = ticks as f64;
    let mut out = rows
        .into_values()
        .filter(|row| owned.contains(&row.pid) && row.state != 'Z')
        .map(|row| {
            let wall_elapsed_s = (boot_s - row.start_ticks as f64 / ticks).max(0.0);
            let cpu_elapsed_s = (row.utime_ticks + row.stime_ticks) as f64 / ticks;
            let signature = process_signature(row.state, wall_elapsed_s, cpu_elapsed_s);
            let test = exact_libtest_from_argv(&row.argv);
            let mut command = if row.argv.is_empty() {
                format!("[pid {}]", row.pid)
            } else {
                row.argv.join(" ")
            };
            if command.len() > 512 {
                command.truncate(512);
                command.push_str("...");
            }
            ProcessObservation {
                pid: row.pid,
                ppid: row.ppid,
                command,
                wall_elapsed_s,
                cpu_elapsed_s,
                signature,
                test_basis: test.as_ref().map(|_| "libtest --exact process argv"),
                test,
            }
        })
        .collect::<Vec<_>>();
    out.sort_by_key(|row| row.pid);
    out
}

/// Use exact per-process libtest bindings only when output supplied no stronger attribution.
///
/// cargo-nextest launches one `--exact TEST` process per test, which makes the binding observable.
/// ordinary `cargo test` runs several tests inside one binary and therefore produces no such row;
/// it remains honestly unattributed unless its output carries boundaries.
pub fn bind_process_tests(mut culprit: Culprit, observations: &[ProcessObservation]) -> Culprit {
    if culprit.test.is_some() || !culprit.in_flight.is_empty() {
        return culprit;
    }
    let mut tests = observations
        .iter()
        .filter_map(|row| {
            row.test.as_ref().map(|test| InFlightTest {
                name: test.clone(),
                elapsed_s: row.wall_elapsed_s,
                basis: row.test_basis.unwrap_or("process observation"),
            })
        })
        .collect::<Vec<_>>();
    tests.sort_by(|a, b| {
        b.elapsed_s
            .partial_cmp(&a.elapsed_s)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    tests.dedup_by(|a, b| a.name == b.name);
    let Some(likely) = tests.first() else {
        return culprit;
    };
    culprit.test = Some(likely.name.clone());
    culprit.how = if tests.len() == 1 {
        "only test-bound process live at termination"
    } else {
        "longest-running test-bound process at termination"
    };
    culprit.in_flight = tests;
    culprit
}

/// How far a step has got, sampled while it is still running.
#[derive(Debug, Clone, Default)]
pub struct StepProgress {
    /// How many tests have begun.
    pub started: u64,
    /// How many tests have finished -- the numerator a reader uses to judge progress.
    pub completed: u64,
    /// The most recent test seen to begin.
    pub last_started: Option<String>,
    /// Tests begun and still running; a parallel harness can have several at once.
    pub in_flight: Vec<String>,
}

impl StepProgress {
    /// True when the step has produced no test events at all, so a caller can fall back to
    /// elapsed-versus-expected rather than printing a row of zeroes that looks like a stall.
    pub fn is_silent(&self) -> bool {
        self.started == 0 && self.completed == 0
    }
}

/// One step's live output state: the durable log, the unterminated tail, and the test tracker.
///
/// stdout and stderr share ONE of these so the tracker sees a single ordered view of the step and
/// a harness that writes progress to stderr is attributed just as well as one that uses stdout.
pub struct StepStream {
    tag: String,
    log: Mutex<Option<File>>,
    /// Bytes received since the last newline. On a hang this holds the hung test's start marker.
    partial: Mutex<String>,
    tracker: Mutex<TestTracker>,
    /// The last START already announced from an UNTERMINATED tail, so it is journaled once.
    tail_announced: Mutex<Option<String>>,
    evidence: Option<std::sync::Arc<RunEvidence>>,
    /// Ceiling on durable bytes for this step's log; `None` = unlimited.
    log_max_bytes: Option<u64>,
    /// Durable bytes written so far, and whether the ceiling has already been announced.
    /// Guarded by the same mutex as `log` so the count cannot drift from the writes.
    written: Mutex<(u64, bool)>,
}

impl StepStream {
    /// Open a step's stream, creating its durable log file when evidence capture is enabled.
    pub fn new(tag: &str, evidence: Option<std::sync::Arc<RunEvidence>>) -> Self {
        Self::with_log_ceiling(tag, evidence, log_max_bytes())
    }

    /// As [`StepStream::new`], with the durable-log ceiling supplied rather than read from the
    /// environment. `None` is unlimited.
    ///
    /// The ceiling is injected rather than read here so the truncation behaviour can be tested
    /// without setting a process-global environment variable, which would race the other tests
    /// running on parallel threads and make this suite order-dependent.
    pub fn with_log_ceiling(
        tag: &str,
        evidence: Option<std::sync::Arc<RunEvidence>>,
        log_max_bytes: Option<u64>,
    ) -> Self {
        let log = evidence.as_ref().and_then(|e| e.open_step_log(tag));
        StepStream {
            tag: tag.to_string(),
            log: Mutex::new(log),
            partial: Mutex::new(String::new()),
            tracker: Mutex::new(TestTracker::default()),
            tail_announced: Mutex::new(None),
            evidence,
            log_max_bytes,
            written: Mutex::new((0, false)),
        }
    }

    /// A live snapshot of how far this step has got, for periodic progress output.
    ///
    /// WHY THIS EXISTS. At the default verbosity the runner emits one START line per step and
    /// then nothing until the step ends, so a phase that legitimately takes minutes is
    /// indistinguishable from a hang -- read as hung, someone kills a healthy run; read as
    /// healthy, a real hang burns to its deadline. Measured on one real graph: 26 of 56 nodes
    /// exceeded 30s and 89% of all node time was spent inside them, so most of the run was
    /// unobservable and its latency could not be decomposed.
    ///
    /// This reports COUNTS, not liveness. "41 done, 3 in flight" tells a reader whether the
    /// step is progressing and roughly how much remains; a bare "still working" tick only
    /// distinguishes alive from dead, which was never the question.
    pub fn progress(&self) -> StepProgress {
        let tracker = self.tracker.lock().unwrap();
        StepProgress {
            started: tracker.started,
            completed: tracker.completed,
            last_started: tracker.last_started.clone(),
            in_flight: tracker.in_flight.keys().cloned().collect(),
        }
    }

    /// Absorb a chunk of raw output: write it through to disk, then split and classify lines.
    pub fn ingest(&self, bytes: &[u8]) {
        // DURABLE CAPTURE IS BOUNDED; CLASSIFICATION IS NOT. Everything below this block --
        // line splitting, test recognition, the unterminated-tail marker -- runs on every byte
        // regardless of the ceiling. A step that floods stdout still gets its hung test named;
        // it just stops being copied to disk. Bounding the disk must not cost the attribution
        // the disk was there to support.
        let mut truncated_now = false;
        if let Ok(mut guard) = self.log.lock() {
            if let Some(f) = guard.as_mut() {
                let mut w = self.written.lock().expect("written mutex poisoned");
                let (count, announced) = &mut *w;
                match self.log_max_bytes {
                    None => {
                        let _ = f.write_all(bytes);
                        let _ = f.flush();
                        *count = count.saturating_add(bytes.len() as u64);
                    }
                    Some(limit) if *count < limit => {
                        // Write only the prefix that fits, so the ceiling is exact rather than
                        // "the last chunk that crossed it", which would make the bound depend on
                        // the reader's buffer size and differ between the two engines.
                        let room = (limit - *count) as usize;
                        let take = room.min(bytes.len());
                        let _ = f.write_all(&bytes[..take]);
                        *count += take as u64;
                        if *count >= limit && !*announced {
                            let _ = f.write_all(TRUNCATION_MARKER.as_bytes());
                            *announced = true;
                            truncated_now = true;
                        }
                        let _ = f.flush();
                    }
                    Some(_) => {
                        // At or past the ceiling and already announced: drop the bytes.
                    }
                }
            }
        }
        // Journal the truncation OUTSIDE the log lock. A capped log that does not say it is
        // capped is a silently incomplete evidence file, which is worse than no evidence file:
        // a later reader cannot tell "the step printed nothing more" from "we stopped writing".
        if truncated_now {
            if let Some(e) = &self.evidence {
                e.record(
                    "step_log_truncated",
                    &[
                        ("step", self.tag.clone()),
                        (
                            "limit_bytes",
                            self.log_max_bytes.unwrap_or_default().to_string(),
                        ),
                    ],
                );
            }
        }
        let text = String::from_utf8_lossy(bytes);
        let mut partial = match self.partial.lock() {
            Ok(p) => p,
            Err(_) => return,
        };
        partial.push_str(&text);
        // Split off every COMPLETE line, leaving the unterminated remainder in `partial`.
        while let Some(idx) = partial.find('\n') {
            let line: String = partial.drain(..=idx).collect();
            drop(partial);
            self.classify(line.trim_end_matches(['\n', '\r']));
            partial = match self.partial.lock() {
                Ok(p) => p,
                Err(_) => return,
            };
        }
        // Bound the retained tail. A step that streams megabytes without a newline (a progress bar
        // rewriting with \r) must not grow this without limit; the marker we care about is short
        // and sits at the END, so keep the tail.
        if partial.len() > 8192 {
            let keep = partial.len() - 4096;
            let cut = (keep..partial.len())
                .find(|i| partial.is_char_boundary(*i))
                .unwrap_or(partial.len());
            let tail = partial[cut..].to_string();
            *partial = tail;
        }
        let tail = partial.clone();
        drop(partial);
        // ANNOUNCE AN UNTERMINATED START IMMEDIATELY, DO NOT WAIT FOR TEARDOWN. This is the
        // record that makes the journal survive a kill of the RUNNER ITSELF (a CI job
        // cancellation, an OOM, a SIGKILL from an outer supervisor) rather than only a kill of
        // the step: at that point nothing computes a verdict, and the newest `test_start` with no
        // matching `test_end` is the whole answer. It is journaled once per distinct marker.
        if let Some(TestEvent::Start(name)) = recognize(&tail) {
            let already = self
                .tail_announced
                .lock()
                .map(|g| g.as_deref() == Some(name.as_str()))
                .unwrap_or(true);
            if !already {
                if let Ok(mut g) = self.tail_announced.lock() {
                    *g = Some(name.clone());
                }
                if let Ok(mut t) = self.tracker.lock() {
                    t.observe(&TestEvent::Start(name.clone()));
                }
                if let Some(e) = &self.evidence {
                    e.record("test_start", &[("step", self.tag.clone()), ("test", name)]);
                }
            }
        }
    }

    fn classify(&self, line: &str) {
        let Some(ev) = recognize(line) else { return };
        // A line that COMPLETES a marker already announced from the tail clears the announcement,
        // so the same test is not counted as started twice.
        if let Ok(mut g) = self.tail_announced.lock() {
            let name = match &ev {
                TestEvent::Start(n) => n,
                TestEvent::End(n, _) => n,
            };
            if g.as_deref() == Some(name.as_str()) {
                *g = None;
                if let TestEvent::End(n, verdict) = &ev {
                    // Already counted as started; record only the completion.
                    if let Ok(mut t) = self.tracker.lock() {
                        t.in_flight.remove(n);
                        t.last_completed = Some(n.clone());
                        t.completed += 1;
                    }
                    drop(g);
                    if let Some(e) = &self.evidence {
                        e.record(
                            "test_end",
                            &[
                                ("step", self.tag.clone()),
                                ("test", n.clone()),
                                ("verdict", verdict.clone()),
                            ],
                        );
                    }
                    return;
                }
                return; // a duplicate START announcement
            }
        }
        if let Ok(mut t) = self.tracker.lock() {
            t.observe(&ev);
        }
        // Emit the boundary to the journal IMMEDIATELY. This is the record that survives a kill:
        // the last `test_start` with no matching `test_end` names the culprit even if the runner
        // itself is SIGKILLed before it can compute a verdict.
        if let Some(e) = &self.evidence {
            match ev {
                TestEvent::Start(name) => {
                    e.record("test_start", &[("step", self.tag.clone()), ("test", name)])
                }
                TestEvent::End(name, verdict) => e.record(
                    "test_end",
                    &[
                        ("step", self.tag.clone()),
                        ("test", name),
                        ("verdict", verdict),
                    ],
                ),
            }
        }
    }

    /// Flush any unterminated tail into the tracker and return the attribution verdict.
    ///
    /// Called at teardown. The tail is classified here and NOT during `ingest`, because a partial
    /// line is only meaningful once we know no more bytes are coming — mid-stream it is simply an
    /// incomplete line that the next chunk will finish.
    pub fn culprit(&self) -> Culprit {
        // Trim only the LEADING side and hard line endings: the trailing space in
        // `test name ... ` is the signal, not noise.
        let tail = self
            .partial
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default()
            .trim_start()
            .trim_end_matches(['\n', '\r'])
            .to_string();
        let tail_event = if tail.trim().is_empty() {
            None
        } else {
            recognize(&tail)
        };
        let t = match self.tracker.lock() {
            Ok(t) => t.clone(),
            Err(_) => TestTracker::default(),
        };
        let in_flight = t.in_flight_snapshot();
        // Explicit boundaries are authoritative for a controlled parallel harness. Report the
        // whole live set with elapsed times; if several remain, the longest-running one is only a
        // LIKELY culprit and the wording says so.
        if let Some(likely) = in_flight.first() {
            return Culprit {
                test: Some(likely.name.clone()),
                how: if in_flight.len() == 1 {
                    "declared in flight and never completed"
                } else {
                    "longest-running of the declared in-flight tests"
                },
                completed: t.completed,
                last_completed: t.last_completed,
                in_flight,
            };
        }
        // 1) An unterminated START marker in the tail is the strongest evidence there is: the
        //    harness announced the test and never got to print its verdict.
        if let Some(TestEvent::Start(name)) = tail_event {
            return Culprit {
                test: Some(name),
                how: "announced in the unterminated output tail, never completed",
                completed: t.completed,
                last_completed: t.last_completed,
                in_flight: Vec::new(),
            };
        }
        // 2) Otherwise a test that started and never ended.
        if let Some(started) = t.last_started.clone() {
            if t.last_completed.as_deref() != Some(started.as_str()) {
                return Culprit {
                    test: Some(started),
                    how: "started and never completed",
                    completed: t.completed,
                    last_completed: t.last_completed,
                    in_flight: Vec::new(),
                };
            }
        }
        // 3) Every started test completed: the hang is after the last one (harness teardown, a
        //    leaked background process, or a harness we cannot parse). Name the boundary, not a
        //    test, because guessing here would be a false accusation.
        Culprit {
            test: None,
            how: if t.completed == 0 {
                "no test boundaries were recognized in this step's output"
            } else {
                "every recognized test completed; the step hung after the last one"
            },
            completed: t.completed,
            last_completed: t.last_completed,
            in_flight: Vec::new(),
        }
    }

    /// Snapshot of the tracker, for callers that want the raw counts.
    pub fn counts(&self) -> TestTracker {
        self.tracker
            .lock()
            .map(|t| t.clone())
            .unwrap_or_else(|_| TestTracker::default())
    }
}

/// Columns describing attribution, for callers that want them in a map.
///
/// Deliberately NOT part of the per-step profile CSV: that schema is asserted byte-identical
/// across engines, so widening it here would break a cross-check that exists to catch exactly
/// that kind of one-sided drift.
pub fn culprit_columns(c: &Culprit) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("culprit_test".into(), c.test.clone().unwrap_or_default());
    m.insert("culprit_basis".into(), c.how.into());
    m.insert("tests_completed".into(), c.completed.to_string());
    m.insert(
        "last_completed_test".into(),
        c.last_completed.clone().unwrap_or_default(),
    );
    m.insert("in_flight_count".into(), c.in_flight.len().to_string());
    m.insert(
        "in_flight_tests".into(),
        c.in_flight
            .iter()
            .map(|t| format!("{}@{:.3}s", t.name, t.elapsed_s))
            .collect::<Vec<_>>()
            .join(","),
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn a_runnable_cpu_consumer_stays_cpu_burning_under_host_contention() {
        assert_eq!(process_signature('R', 1.81, 0.83), "cpu-burning");
        assert_eq!(process_signature('S', 1.81, 0.83), "mixed-or-too-young");
        assert_eq!(process_signature('S', 1.81, 0.01), "wall-stalled");
    }

    fn temp_evidence(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dagrun-evidence-{name}-{}-{}",
            std::process::id(),
            NONCE_SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn fifo(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the CString is NUL-terminated and valid for the duration of mkfifo.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn the_default_capture_ceiling_is_four_mebibytes_literally() {
        // The DEFAULT is pinned to a LITERAL, because an assertion written in terms of the
        // constant is circular: it holds for every value the constant could ever have, so
        // shrinking the ceiling by three orders of magnitude -- the difference between "the tail
        // a human reads" and "a few lines" -- passes. The sibling engine pins the same literal,
        // so the two defaults cannot drift apart without one of the two failing by name; nothing
        // else compares them, because the differential sets the environment override and so never
        // observes the default at all.
        assert_eq!(DEFAULT_CAPTURE_MAX_BYTES, 4_194_304);
    }

    #[test]
    fn step_end_reader_refuses_string_verdicts() {
        assert_eq!(
            require_step_end_ok(&serde_json::json!({"event": "step_end", "ok": false})),
            Ok(false)
        );
        assert_eq!(
            require_step_end_ok(&serde_json::json!({"event": "step_end", "ok": true})),
            Ok(true)
        );
        for malformed in [
            serde_json::json!("false"),
            serde_json::json!("true"),
            serde_json::json!(0),
            serde_json::Value::Null,
        ] {
            let error =
                require_step_end_ok(&serde_json::json!({"event": "step_end", "ok": malformed}))
                    .expect_err("a non-boolean verdict must be refused");
            assert!(error.contains("ok must be a boolean"), "{error}");
        }
    }

    #[test]
    fn step_log_is_bounded_exactly_and_says_so() {
        // A capped log that does not say it is capped is a silently incomplete evidence file:
        // a later reader cannot tell "the step printed nothing more" from "we stopped writing".
        let dir = temp_evidence("cap");
        let evidence = std::sync::Arc::new(RunEvidence::open(Some(dir.clone())).unwrap());
        let stream = StepStream::with_log_ceiling("g.j", Some(evidence), Some(100));
        stream.ingest(&vec![b'A'; 5000]);

        let log = std::fs::read(dir.join("g.j.log")).unwrap();
        // EXACT: 100 payload bytes, not "the chunk that crossed 100". A ceiling that depended on
        // the reader's buffer size would differ between the engines and break `make cross`.
        let mut want = vec![b'A'; 100];
        want.extend_from_slice(TRUNCATION_MARKER.as_bytes());
        assert_eq!(log, want);

        let journal = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
        assert_eq!(journal.matches("\"step_log_truncated\"").count(), 1);
        assert!(journal.contains("\"limit_bytes\":\"100\""));

        // Announced ONCE, however many further chunks arrive.
        stream.ingest(&vec![b'B'; 5000]);
        assert_eq!(std::fs::read(dir.join("g.j.log")).unwrap(), want);
        let journal = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
        assert_eq!(journal.matches("\"step_log_truncated\"").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classification_survives_a_truncated_log() {
        // Bounding the DISK must not cost the attribution the disk was there to support.
        let dir = temp_evidence("cap-cls");
        let evidence = std::sync::Arc::new(RunEvidence::open(Some(dir.clone())).unwrap());
        let stream = StepStream::with_log_ceiling("g.j", Some(evidence), Some(10));
        let mut flood = vec![b'A'; 500];
        flood.push(b'\n');
        stream.ingest(&flood);
        stream.ingest(b"test mymod::mytest ... ");

        let journal = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
        assert!(journal.contains("\"step_log_truncated\""));
        assert!(
            journal.contains("\"test\":\"mymod::mytest\""),
            "the hung test must still be named after the log stopped being written: {journal}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unlimited_ceiling_writes_everything() {
        let dir = temp_evidence("cap-none");
        let evidence = std::sync::Arc::new(RunEvidence::open(Some(dir.clone())).unwrap());
        let stream = StepStream::with_log_ceiling("g.j", Some(evidence), None);
        stream.ingest(&vec![b'A'; 5000]);
        assert_eq!(std::fs::read(dir.join("g.j.log")).unwrap().len(), 5000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn libtest_completed_line_is_an_end() {
        assert_eq!(
            recognize("test detcore::sched::race ... ok"),
            Some(TestEvent::End("detcore::sched::race".into(), "ok".into()))
        );
        assert_eq!(
            recognize("test detcore::sched::race ... FAILED"),
            Some(TestEvent::End(
                "detcore::sched::race".into(),
                "FAILED".into()
            ))
        );
    }

    #[test]
    fn libtest_bare_prefix_is_a_start() {
        assert_eq!(
            recognize("test detcore::sched::hang ... "),
            Some(TestEvent::Start("detcore::sched::hang".into()))
        );
    }

    #[test]
    fn pytest_and_tap_and_explicit_protocol() {
        assert_eq!(
            recognize("tests/test_a.py::test_b PASSED"),
            Some(TestEvent::End(
                "tests/test_a.py::test_b".into(),
                "PASSED".into()
            ))
        );
        assert_eq!(
            recognize("ok 7 - the seventh thing"),
            Some(TestEvent::End("the seventh thing".into(), "ok".into()))
        );
        assert_eq!(
            recognize("not ok 8 - the eighth thing"),
            Some(TestEvent::End("the eighth thing".into(), "not ok".into()))
        );
        assert_eq!(
            recognize("##TEST-START my_case"),
            Some(TestEvent::Start("my_case".into()))
        );
        assert_eq!(
            recognize("##TEST-END my_case ok"),
            Some(TestEvent::End("my_case".into(), "ok".into()))
        );
    }

    #[test]
    fn ordinary_output_is_not_mistaken_for_a_test() {
        // A false positive renames the culprit, so these must all stay unrecognized.
        for line in [
            "",
            "Compiling widget v0.1.0",
            "test suite finished",
            "ok",
            "ok then, moving on",
            "running 42 tests",
            "note: test failed, to rerun pass `--lib`",
        ] {
            assert_eq!(recognize(line), None, "misrecognized: {line:?}");
        }
    }

    #[test]
    fn culprit_prefers_the_unterminated_tail() {
        let s = StepStream::new("g:j", None);
        s.ingest(b"test a::one ... ok\ntest a::two ... ok\ntest a::hangs ... ");
        let c = s.culprit();
        assert_eq!(c.test.as_deref(), Some("a::hangs"));
        assert_eq!(c.completed, 2);
        assert_eq!(c.last_completed.as_deref(), Some("a::two"));
    }

    #[test]
    fn culprit_uses_started_without_end_when_the_tail_is_clean() {
        let s = StepStream::new("g:j", None);
        s.ingest(b"##TEST-START alpha\n##TEST-END alpha ok\n##TEST-START beta\nnoise\n");
        let c = s.culprit();
        assert_eq!(c.test.as_deref(), Some("beta"));
        assert_eq!(c.completed, 1);
    }

    #[test]
    fn parallel_explicit_boundaries_report_the_complete_live_set_and_elapsed_time() {
        let s = StepStream::new("g:j", None);
        s.ingest(b"##TEST-START suite::older\n");
        std::thread::sleep(std::time::Duration::from_millis(20));
        s.ingest(b"##TEST-START suite::newer\n");
        let c = s.culprit();
        assert_eq!(c.test.as_deref(), Some("suite::older"));
        assert_eq!(c.in_flight.len(), 2);
        assert_eq!(c.in_flight[0].name, "suite::older");
        assert!(c.in_flight[0].elapsed_s > c.in_flight[1].elapsed_s);
        assert!(c.describe().contains("likely culprit test suite::older"));

        s.ingest(b"##TEST-END suite::older ok\n");
        let c = s.culprit();
        assert_eq!(c.test.as_deref(), Some("suite::newer"));
        assert_eq!(c.in_flight.len(), 1);
    }

    #[test]
    fn exact_libtest_process_binding_accepts_both_argument_orders_without_guessing() {
        assert_eq!(
            exact_libtest_from_argv(&[
                "/tmp/test-bin".into(),
                "--exact".into(),
                "suite::case".into(),
                "--nocapture".into(),
            ]),
            Some("suite::case".into())
        );
        assert_eq!(
            exact_libtest_from_argv(&[
                "/tmp/test-bin".into(),
                "suite::case".into(),
                "--exact".into(),
            ]),
            Some("suite::case".into())
        );
        assert_eq!(
            exact_libtest_from_argv(&["/tmp/test-bin".into(), "--test-threads=4".into(),]),
            None
        );
    }

    #[test]
    fn a_clean_suite_accuses_nobody() {
        let s = StepStream::new("g:j", None);
        s.ingest(b"test a::one ... ok\ntest a::two ... ok\ntest result: ok. 2 passed\n");
        let c = s.culprit();
        assert_eq!(c.test, None);
        assert_eq!(c.completed, 2);
        assert_eq!(c.last_completed.as_deref(), Some("a::two"));
    }

    #[test]
    fn unparsable_output_degrades_to_naming_no_test() {
        let s = StepStream::new("g:j", None);
        s.ingest(b"building...\nlinking...\n");
        let c = s.culprit();
        assert_eq!(c.test, None);
        assert_eq!(c.completed, 0);
    }

    #[test]
    fn partial_tail_is_bounded() {
        let s = StepStream::new("g:j", None);
        let blob = vec![b'x'; 100_000];
        s.ingest(&blob);
        assert!(s.partial.lock().unwrap().len() <= 8192);
    }

    #[test]
    fn nonces_are_unique_per_step() {
        let a = mint_step_nonce("g:j");
        let b = mint_step_nonce("g:j");
        assert_ne!(a, b);
        assert!(a.ends_with("g~3aj"));
    }

    #[test]
    fn sanitized_names_are_injective_for_former_aliases() {
        assert_ne!(sanitize("a/b.j"), sanitize("a_b.j"));
        assert_ne!(sanitize("a/b.j"), sanitize("a~2fb.j"));
    }

    #[test]
    fn evidence_directory_and_files_are_private() {
        let dir = temp_evidence("private");
        let evidence = RunEvidence::open(Some(dir.clone())).expect("private evidence opens");
        evidence.record("test", &[]);
        let mut step = evidence.open_step_log("g:j").expect("step log opens");
        step.write_all(b"secret\n").unwrap();

        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for file in [dir.join("journal.jsonl"), dir.join("g~3aj.log")] {
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(step);
        drop(evidence);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn evidence_refuses_public_or_symlink_directories() {
        let public = temp_evidence("public");
        std::fs::create_dir(&public).unwrap();
        std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(RunEvidence::open(Some(public.clone())).is_none());

        let target = temp_evidence("target");
        let link = temp_evidence("link");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, &link).unwrap();
        assert!(RunEvidence::open(Some(link.clone())).is_none());

        let _ = std::fs::remove_file(link);
        let _ = std::fs::remove_dir(target);
        let _ = std::fs::remove_dir(public);
    }

    #[test]
    fn step_log_refuses_a_hard_link_without_truncating_its_target() {
        let dir = temp_evidence("hard-link");
        let evidence = RunEvidence::open(Some(dir.clone())).expect("private evidence opens");
        let victim = dir.join("victim");
        std::fs::write(&victim, b"keep me").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&victim, dir.join("g~3aj.log")).unwrap();

        assert!(evidence.open_step_log("g:j").is_none());
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
        drop(evidence);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn evidence_refuses_fifos_without_blocking() {
        let journal_dir = temp_evidence("journal-fifo");
        std::fs::create_dir(&journal_dir).unwrap();
        std::fs::set_permissions(&journal_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        fifo(&journal_dir.join("journal.jsonl"));
        assert!(RunEvidence::open(Some(journal_dir.clone())).is_none());

        let step_dir = temp_evidence("step-fifo");
        let evidence = RunEvidence::open(Some(step_dir.clone())).expect("evidence opens");
        fifo(&step_dir.join("g~3aj.log"));
        assert!(evidence.open_step_log("g:j").is_none());

        drop(evidence);
        let _ = std::fs::remove_dir_all(journal_dir);
        let _ = std::fs::remove_dir_all(step_dir);
    }

    #[test]
    fn evidence_refuses_overlong_step_log_names() {
        let dir = temp_evidence("long-name");
        let evidence = RunEvidence::open(Some(dir.clone())).expect("evidence opens");
        assert!(evidence.open_step_log(&"/".repeat(100)).is_none());

        drop(evidence);
        let _ = std::fs::remove_dir_all(dir);
    }
}
