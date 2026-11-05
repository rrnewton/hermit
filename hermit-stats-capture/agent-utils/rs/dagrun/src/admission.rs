//! Host-wide MEMORY admission: grant, queue, or refuse a run before it contends.
//
// WHAT THIS ADDS THAT `--max-mem` DOES NOT. `sizing::box_mem_budget_bytes` and the `--max-mem`
// refusal in the CLI gate memory at box bring-up: one process, one question, one yes-or-no,
// answered against a snapshot of the host. That has no notion of what OTHER runner invocations on
// the same host have already committed to, so two boxes started a second apart each see the same
// headroom and both take it. Neither is wrong on its own; together they overcommit the machine and
// the symptom is swapping, or an OOM kill in whichever run happens to touch its pages last.
//
// This module is the missing shared state, and it is deliberately the SIBLING of the core ledger
// in `reservation.rs`: a durable, `flock`-serialized file every runner on the host contends on,
// with dead-holder reclaim fingerprinted by `(pid, /proc starttime)` so a crashed run cannot
// subtract memory forever and a recycled PID is never mistaken for the original holder.
//
// THREE ANSWERS, NOT TWO. A yes-or-no can only ever say "no", which tells the caller nothing about
// what to do next: GRANT holds the reservation, QUEUE means waiting can help and says how many
// holders are ahead, REFUSE means waiting can NEVER help and says the number to ask for instead.
// Collapsing QUEUE into REFUSE turns a transient into a permanent failure; collapsing REFUSE into
// QUEUE turns a configuration error into a hang.
//
// TWO CONDITIONS, EACH NAMED SEPARATELY. A grant requires `reserved + requested <= whole-host
// budget` (the condition other RUNNERS affect) AND `requested <= live headroom` (the condition
// NON-RUNNER tenants affect: a ledger cannot see a database that grew). They are kept apart
// because the remedies differ.
//
// The ledger schema, the environment variables, the budget arithmetic and the rendered verdict
// text are shared with the Python distribution byte-for-byte. Change one engine only and
// `cross/differential.py` fails.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Number, Value};

/// Environment override for the shared memory ledger path (sibling of `DAGRUN_CORE_LEDGER`).
pub const MEM_LEDGER_ENV: &str = "DAGRUN_MEM_LEDGER";

/// Environment override for the whole-host aggregate budget, in BYTES.
///
/// An operator knob, not a test hook: on a shared machine the fraction-of-MemTotal default is a
/// guess about how much of the box this tool may claim, and the person who owns the machine knows
/// better. An unparseable value is reported and ignored rather than silently taken as zero, which
/// would refuse every run.
pub const MEM_BUDGET_BYTES_ENV: &str = "DAGRUN_ADMISSION_BUDGET_BYTES";

/// Environment override for the live-headroom reading, in BYTES.
pub const MEM_HEADROOM_BYTES_ENV: &str = "DAGRUN_ADMISSION_HEADROOM_BYTES";

/// Fraction of `MemTotal` this tool will let its runs hold IN AGGREGATE.
///
/// Not 1.0, and the gap is not timidity: the kernel, the page cache, and whatever else the machine
/// exists to do all need memory, and a runner that plans to the last byte is planning for the OOM
/// killer to arbitrate.
pub const DEFAULT_MEM_BUDGET_FRACTION: f64 = 0.85;

/// Absolute headroom kept back on top of the fraction.
///
/// A fraction alone scales the wrong way: 15% of a 512 GiB host is 76 GiB of slack nobody needs,
/// while 15% of an 8 GiB host is 1.2 GiB, which one page-cache spike erases.
pub const MEM_SAFETY_MARGIN_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// The flat margin is never allowed to exceed this share of the measure it is taken from.
///
/// An UNCAPPED flat margin is a gate that never opens. Held back whole, 8 GiB of an 8 GiB machine
/// leaves an aggregate budget of ZERO: every request is REFUSED, and the refusal helpfully advises
/// asking for "at most 0 B" -- while the live headroom reading is pinned at zero too, so nothing
/// can queue its way in either. That is not a conservative gate, it is a broken one, and it breaks
/// precisely on the small hosts the flat margin was added to protect.
pub const MEM_SAFETY_MARGIN_MAX_DIVISOR: u64 = 8;

/// Longest wait [`admit`] will honour, however large `wait_s` is.
///
/// A year is not a wait, it is a hang with a timestamp -- but the reason this exists is
/// arithmetic: `Instant + Duration` PANICS on overflow, so an unbounded seconds value aborts the
/// process (exit 101) on a number the caller has just been told is acceptable. Every paired
/// implementation of this runner clamps to the same ceiling, so an absurd wait folds to one
/// finite answer in all of them.
pub const MAX_WAIT_SECONDS: u64 = 365 * 24 * 60 * 60;

const MAX_BYTES: u64 = 1 << 62;

/// The flat margin, capped so it can never consume the whole of `scale`.
///
/// From 64 GiB upward the cap is not binding and the margin is the flat 8 GiB. Below that it
/// scales down with the host, so the budget stays a positive share of the machine at every size
/// instead of collapsing to zero and refusing everything.
fn safety_margin_bytes(scale: u64) -> u64 {
    MEM_SAFETY_MARGIN_BYTES.min(scale / MEM_SAFETY_MARGIN_MAX_DIVISOR)
}

/// The aggregate budget implied by a host of `total` bytes.
///
/// Split out from [`host_budget_bytes`] so the ARITHMETIC can be pinned against literal numbers
/// without a readable `/proc/meminfo`. Every paired implementation carries the same function and
/// pins the same literals: the fraction is a production constant the paired engines share through
/// one ledger, and a constant no test names is a constant either engine can shrink unnoticed.
fn budget_from_total(total: u64) -> u64 {
    let scaled = (total as f64 * DEFAULT_MEM_BUDGET_FRACTION) as u64;
    scaled.saturating_sub(safety_margin_bytes(total))
}

/// The live headroom implied by `available` bytes free on a host of `scale` bytes.
fn headroom_from(available: u64, scale: u64) -> u64 {
    available.saturating_sub(safety_margin_bytes(scale))
}

/// The three answers admission can give. The values are part of the printed contract, so every
/// paired implementation of this runner spells them identically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The reservation is recorded and held for the life of the run.
    Grant,
    /// It would fit on a quiet host, so WAITING CAN HELP.
    Queue,
    /// The request alone exceeds the whole-host budget, so waiting can NEVER help.
    Refuse,
}

impl Verdict {
    /// The wire spelling of this verdict.
    ///
    /// It is part of the printed contract, not a debug label: every paired implementation of
    /// this runner spells the three answers identically, because they share one ledger.
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Grant => "grant",
            Verdict::Queue => "queue",
            Verdict::Refuse => "refuse",
        }
    }
}

/// One admission answer, carrying every number it was made from.
///
/// The numbers are not decoration. A run that waits, or refuses, has to be explainable months
/// later from one line of log, and "admission denied" is not explainable.
#[derive(Clone, Debug)]
pub struct Decision {
    /// Which of the three answers this is.
    pub verdict: Verdict,
    /// The printed explanation, carrying every number the decision was made from.
    pub reason: String,
    /// What was asked for.
    pub requested_bytes: u64,
    /// Aggregate host budget, or `None` when the host could not be measured.
    pub budget_bytes: Option<u64>,
    /// Live headroom, or `None` when the host could not be measured.
    pub headroom_bytes: Option<u64>,
    /// Sum of live reservations in the ledger, this request excluded.
    pub reserved_bytes: u64,
    /// For QUEUE: the fewest live holders that must finish first. Zero otherwise.
    pub holders_ahead: usize,
    /// For REFUSE: the largest request that could ever be granted on this host.
    pub largest_grantable_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
struct Record {
    pid: u32,
    starttime: Option<u64>,
    bytes: u64,
    tag: String,
    ts: f64,
}

impl Record {
    fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        let raw_pid = value_u64(obj.get("pid")?)?;
        let pid = u32::try_from(raw_pid).ok().filter(|pid| *pid > 0)?;
        let starttime = match obj.get("starttime")? {
            Value::Null => None,
            value => Some(value_u64(value).filter(|starttime| *starttime > 0)?),
        };
        let bytes = value_u64(obj.get("bytes")?).filter(|b| *b >= 1 && *b <= MAX_BYTES)?;
        let tag = obj.get("tag")?.as_str()?.to_string();
        let ts = match obj.get("ts")? {
            Value::Number(number) => number.as_f64()?,
            _ => return None,
        };
        if !ts.is_finite() || ts < 0.0 {
            return None;
        }
        Some(Self {
            pid,
            starttime,
            bytes,
            tag,
            ts,
        })
    }

    fn to_value(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("pid".into(), Value::from(self.pid));
        obj.insert(
            "starttime".into(),
            self.starttime.map(Value::from).unwrap_or(Value::Null),
        );
        obj.insert("bytes".into(), Value::from(self.bytes));
        obj.insert("tag".into(), Value::String(self.tag.clone()));
        obj.insert(
            "ts".into(),
            Number::from_f64(self.ts)
                .map(Value::Number)
                .unwrap_or_else(|| Value::from(0)),
        );
        Value::Object(obj)
    }
}

fn value_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// Resolve the host-wide admission-ledger path from the environment or runtime directory.
pub fn default_ledger_path() -> PathBuf {
    if let Some(path) = std::env::var_os(MEM_LEDGER_ENV) {
        return PathBuf::from(path);
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let runtime = PathBuf::from(runtime);
        if runtime.is_dir() {
            return runtime.join("dagrun").join("memory-admissions.json");
        }
    }
    // SAFETY: geteuid has no preconditions and does not access memory.
    let uid = unsafe { libc::geteuid() };
    std::env::temp_dir()
        .join(format!("dagrun-{uid}"))
        .join("memory-admissions.json")
}

/// One `/proc/meminfo` field in bytes, or `None` when it cannot be read.
///
/// ABSENT IS NOT ZERO: an unreadable `/proc/meminfo` means the host's memory is UNKNOWN, not that
/// it has none. Returning 0 would refuse every run on a host this code simply could not measure.
fn meminfo_bytes(key: &str) -> Option<u64> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        let (name, rest) = line.split_once(':')?;
        if name.trim() != key {
            continue;
        }
        let mut parts = rest.split_whitespace();
        let value: u64 = parts.next()?.parse().ok()?;
        let unit = parts.next().unwrap_or("kB").to_ascii_lowercase();
        return Some(if unit == "kb" { value * 1024 } else { value });
    }
    None
}

fn env_bytes(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    // ONE message for both rejections, worded exactly as the Python engine words it. A negative
    // count of bytes and a non-numeric string are the same mistake from the operator's side, and
    // the two editions read and write ONE ledger, so a warning that differs between them is a
    // difference a reader would have to explain.
    match raw.trim().parse::<u64>() {
        Ok(value) => Some(value),
        Err(_) => {
            println!(
                "[dagrun] WARNING: {name}='{raw}' is not a non-negative integer \
                 number of bytes; ignoring it and measuring the host instead."
            );
            None
        }
    }
}

/// The aggregate this tool will ever let its runs hold on this host, or `None` if unknown.
///
/// Re-read on EVERY call rather than cached: a cached budget stops matching the machine it is
/// supposed to describe.
pub fn host_budget_bytes() -> Option<u64> {
    if let Some(override_bytes) = env_bytes(MEM_BUDGET_BYTES_ENV) {
        return Some(override_bytes);
    }
    let total = meminfo_bytes("MemTotal")?;
    Some(budget_from_total(total))
}

/// Memory actually available on the host right now, minus the margin; `None` if unknown.
///
/// This is the term that sees tenants this tool does not manage. A ledger can only account for
/// runs that went through it, so without this reading a host loaded to 99% by something else would
/// still look empty and admission would grant into a machine that is already swapping.
pub fn live_headroom_bytes() -> Option<u64> {
    if let Some(override_bytes) = env_bytes(MEM_HEADROOM_BYTES_ENV) {
        return Some(override_bytes);
    }
    let available = meminfo_bytes("MemAvailable")?;
    // The margin is a property of the HOST, so it is scaled by MemTotal, not by whatever happens
    // to be free at this instant -- otherwise a momentarily busy host would shrink its own margin
    // exactly when the margin matters. MemTotal is readable whenever MemAvailable is; the fallback
    // only keeps this honest if that ever stops being true.
    let scale = meminfo_bytes("MemTotal").unwrap_or(available);
    Some(headroom_from(available, scale))
}

fn proc_starttime(pid: u32) -> Result<Option<u64>, String> {
    crate::procstat::read(pid)
        .map(|stat| stat.map(|stat| stat.starttime_ticks))
        .map_err(|error| error.to_string())
}

fn current_starttime(pid: u32) -> Result<u64, String> {
    proc_starttime(pid)?
        .ok_or_else(|| format!("could not read current process identity from /proc/{pid}/stat"))
}

fn holder_alive(record: &Record) -> Result<bool, String> {
    match proc_starttime(record.pid)? {
        None => Ok(false),
        Some(_) if record.starttime.is_none() => Ok(true),
        Some(current) => Ok(record.starttime == Some(current)),
    }
}

/// Exclusive `flock` over the ledger's critical section.
///
/// THE LOCK IS WHAT MAKES TWO SIMULTANEOUS ADMITS SAFE. It is held across the whole
/// sweep -> measure -> decide -> record section, so a concurrent request blocks and, when it
/// proceeds, sees the first request's reservation already recorded. Without that, both requests
/// read the same "reserved" total and both grant -- the exact defect this module exists to remove,
/// reproduced inside the fix.
struct LedgerLock {
    file: File,
}

impl LedgerLock {
    fn acquire(ledger: &Path) -> Result<Self, String> {
        let lock_path = PathBuf::from(format!("{}.lock", ledger.display()));
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create ledger directory: {e}"))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&lock_path)
            .map_err(|e| format!("open admission lock {}: {e}", lock_path.display()))?;
        validate_private_regular(&file, &lock_path)?;
        // SAFETY: flock operates on this live file descriptor and does not retain a pointer.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "lock admission ledger {}: {}",
                lock_path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { file })
    }
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid until `file` is dropped after this method.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn validate_private_regular(file: &File, path: &Path) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|e| format!("inspect {}: {e}", path.display()))?;
    // SAFETY: geteuid has no preconditions and does not access memory.
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_file() || metadata.uid() != uid || metadata.nlink() != 1 {
        return Err(format!(
            "{} is not an owned, single-link regular file",
            path.display()
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("make {} private: {e}", path.display()))?;
    }
    Ok(())
}

fn load(path: &Path) -> Result<Vec<Record>, String> {
    let mut text = String::new();
    let mut file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("open admission ledger {}: {error}", path.display())),
    };
    validate_private_regular(&file, path)?;
    file.read_to_string(&mut text)
        .map_err(|e| format!("read admission ledger {}: {e}", path.display()))?;
    let Value::Object(root) = serde_json::from_str::<Value>(&text)
        .map_err(|e| format!("admission ledger {} is corrupt: {e}", path.display()))?
    else {
        return Err(format!(
            "admission ledger {} root must be an object",
            path.display()
        ));
    };
    let items = root
        .get("admissions")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("admission ledger {} has no admissions list", path.display()))?;
    items
        .iter()
        .map(|item| {
            Record::from_value(item)
                .ok_or_else(|| format!("admission ledger {} has an invalid record", path.display()))
        })
        .collect()
}

fn store(path: &Path, records: &[Record]) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| format!("create ledger directory: {e}"))?;
    let payload = Value::Object(Map::from_iter([(
        "admissions".to_string(),
        Value::Array(records.iter().map(Record::to_value).collect()),
    )]));
    let bytes = serde_json::to_vec(&payload).map_err(|e| format!("encode ledger: {e}"))?;
    let mut created: Option<(PathBuf, File)> = None;
    for attempt in 0..100u32 {
        let temp = parent.join(format!(
            ".memory-admissions.{}.{}.tmp",
            std::process::id(),
            attempt
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&temp)
        {
            Ok(file) => {
                created = Some((temp, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create temporary ledger: {e}")),
        }
    }
    let (temp, mut file) =
        created.ok_or_else(|| "could not allocate temporary ledger".to_string())?;
    let result = (|| {
        file.write_all(&bytes)
            .map_err(|e| format!("write temporary ledger: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("sync temporary ledger: {e}"))?;
        drop(file);
        fs::rename(&temp, path).map_err(|e| format!("replace admission ledger: {e}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn sweep(records: Vec<Record>) -> Result<(Vec<Record>, Vec<Record>), String> {
    let mut live = Vec::new();
    let mut dead = Vec::new();
    for record in records {
        if holder_alive(&record)? {
            live.push(record);
        } else {
            dead.push(record);
        }
    }
    Ok((live, dead))
}

/// How many live holders must finish before `need` more bytes fit. Largest first.
///
/// A real number, not a ticket counter: it answers "how much has to happen before my turn".
/// Largest-first is the optimistic reading and is honest about being one -- it is the FEWEST that
/// could suffice.
fn holders_that_must_release(live: &[Record], need: u64) -> usize {
    if need == 0 {
        return 0;
    }
    let mut sizes: Vec<u64> = live.iter().map(|r| r.bytes).collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    let mut freed = 0u64;
    for (index, size) in sizes.iter().enumerate() {
        freed = freed.saturating_add(*size);
        if freed >= need {
            return index + 1;
        }
    }
    sizes.len()
}

// Human-readable byte count. MUST match the paired engine's renderer exactly, because the
// rendered verdicts themselves are compared across the two engines: a run that reads "16.0 GiB"
// from one and "16 GiB" from the other is two different printed contracts for one number.
fn fmt_bytes(n: Option<u64>) -> String {
    let Some(n) = n else {
        return "unknown".to_string();
    };
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    for unit in units {
        if value < 1024.0 || unit == "TiB" {
            return if unit == "B" {
                format!("{} B", value as u64)
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{n} B")
}

/// A granted memory reservation. Released on `Drop`, and reclaimed by the next sweep if the
/// holder dies first.
pub struct MemoryReservation {
    bytes: u64,
    pid: u32,
    starttime: Option<u64>,
    tag: String,
    ledger: PathBuf,
    released: bool,
}

impl MemoryReservation {
    /// Return this run's share of the host budget. Idempotent.
    pub fn release(&mut self) -> Result<(), String> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        let _lock = LedgerLock::acquire(&self.ledger)?;
        let records = load(&self.ledger)?;
        let kept: Vec<Record> = records
            .into_iter()
            .filter(|r| {
                !(r.pid == self.pid
                    && r.starttime == self.starttime
                    && r.tag == self.tag
                    && r.bytes == self.bytes)
            })
            .collect();
        store(&self.ledger, &kept)
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// The wait [`admit`] will actually honour: negative and NaN fold to none, absurd to the cap.
///
/// This is the line that used to abort the process. `Duration::from_secs_f64` panics above
/// `u64::MAX` seconds, and `Instant + Duration` panics on overflow, so a wait value the CLI had
/// just validated could kill the run with exit 101 instead of returning a verdict. Every paired
/// implementation folds the same three cases to the same three answers.
fn wait_budget(wait_s: f64) -> Duration {
    let cap = Duration::from_secs(MAX_WAIT_SECONDS);
    Duration::try_from_secs_f64(wait_s.max(0.0))
        .unwrap_or(cap)
        .min(cap)
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Ask for `requested_bytes` of the host budget ONCE, without waiting.
///
/// The entire sweep -> measure -> decide -> record sequence runs under the exclusive ledger lock,
/// so two overlapping requests cannot both see the same free budget and both grant.
pub fn request(
    requested_bytes: u64,
    tag: &str,
    ledger: Option<&Path>,
) -> Result<(Decision, Option<MemoryReservation>), String> {
    request_with_limits(
        requested_bytes,
        tag,
        ledger,
        host_budget_bytes(),
        live_headroom_bytes(),
    )
}

/// [`request`] with the two host limits supplied rather than measured.
///
/// The split exists so the DECISION can be exercised against exact numbers. Reading the limits
/// from the environment inside the critical section makes the rule untestable without mutating
/// process-global state, which under a parallel test runner is a race rather than a fixture --
/// and a racy test of an admission rule is worse than none, because it fails for the wrong reason.
pub fn request_with_limits(
    requested_bytes: u64,
    tag: &str,
    ledger: Option<&Path>,
    budget: Option<u64>,
    headroom: Option<u64>,
) -> Result<(Decision, Option<MemoryReservation>), String> {
    if requested_bytes < 1 {
        return Err("requested_bytes must be >= 1".to_string());
    }
    let path = match ledger {
        Some(p) => p.to_path_buf(),
        None => default_ledger_path(),
    };
    // SAFETY: getpid has no preconditions and does not access memory.
    let pid = std::process::id();
    let starttime = Some(current_starttime(pid)?);

    let decision;
    let granted;
    {
        let _lock = LedgerLock::acquire(&path)?;
        let records = load(&path)?;
        let (live, dead) = sweep(records)?;
        if !dead.is_empty() {
            store(&path, &live)?;
        }
        let reserved: u64 = live.iter().map(|r| r.bytes).sum();

        if let Some(budget) = budget {
            if requested_bytes > budget {
                // REFUSE, not QUEUE: no amount of waiting makes the host bigger.
                return Ok((
                    Decision {
                        verdict: Verdict::Refuse,
                        reason: format!(
                            "REFUSED: {} exceeds the whole-host budget of {}, so waiting can \
                             never help. Ask for at most {} ({budget} bytes), or raise the budget \
                             with {MEM_BUDGET_BYTES_ENV}.",
                            fmt_bytes(Some(requested_bytes)),
                            fmt_bytes(Some(budget)),
                            fmt_bytes(Some(budget)),
                        ),
                        requested_bytes,
                        budget_bytes: Some(budget),
                        headroom_bytes: headroom,
                        reserved_bytes: reserved,
                        holders_ahead: 0,
                        largest_grantable_bytes: Some(budget),
                    },
                    None,
                ));
            }
            if reserved.saturating_add(requested_bytes) > budget {
                let need = reserved + requested_bytes - budget;
                let ahead = holders_that_must_release(&live, need);
                return Ok((
                    Decision {
                        verdict: Verdict::Queue,
                        reason: format!(
                            "QUEUED on OTHER RUNS: {} would fit on a quiet host, but {} live \
                             reservation(s) already hold {} of the {} budget. Waiting on {ahead} \
                             holder(s) to finish.",
                            fmt_bytes(Some(requested_bytes)),
                            live.len(),
                            fmt_bytes(Some(reserved)),
                            fmt_bytes(Some(budget)),
                        ),
                        requested_bytes,
                        budget_bytes: Some(budget),
                        headroom_bytes: headroom,
                        reserved_bytes: reserved,
                        holders_ahead: ahead,
                        largest_grantable_bytes: Some(budget),
                    },
                    None,
                ));
            }
        }

        if let Some(headroom) = headroom {
            if requested_bytes > headroom {
                return Ok((
                    Decision {
                        verdict: Verdict::Queue,
                        reason: format!(
                            "QUEUED on HOST MEMORY held outside this tool: {} is within the {} \
                             budget, but only {} is actually available right now. The ledger \
                             cannot see a non-runner tenant; this reading can.",
                            fmt_bytes(Some(requested_bytes)),
                            fmt_bytes(budget),
                            fmt_bytes(Some(headroom)),
                        ),
                        requested_bytes,
                        budget_bytes: budget,
                        headroom_bytes: Some(headroom),
                        reserved_bytes: reserved,
                        // Nothing in the ledger is in the way, so no holder finishing will help.
                        holders_ahead: 0,
                        largest_grantable_bytes: budget,
                    },
                    None,
                ));
            }
        }

        let mut next = live.clone();
        next.push(Record {
            pid,
            starttime,
            bytes: requested_bytes,
            tag: tag.to_string(),
            ts: now_secs(),
        });
        store(&path, &next)?;
        decision = Decision {
            verdict: Verdict::Grant,
            reason: format!(
                "GRANTED {} (host budget {}, {} already reserved by {} other run(s))",
                fmt_bytes(Some(requested_bytes)),
                fmt_bytes(budget),
                fmt_bytes(Some(reserved)),
                live.len(),
            ),
            requested_bytes,
            budget_bytes: budget,
            headroom_bytes: headroom,
            reserved_bytes: reserved,
            holders_ahead: 0,
            largest_grantable_bytes: budget,
        };
        granted = Some(MemoryReservation {
            bytes: requested_bytes,
            pid,
            starttime,
            tag: tag.to_string(),
            ledger: path.clone(),
            released: false,
        });
    }
    Ok((decision, granted))
}

/// Request admission, WAITING while the answer is QUEUE, up to `wait_s` seconds.
///
/// A WAITING RUN SAYS SO, and says it again whenever the answer changes. A queued run that printed
/// nothing is indistinguishable from a wedged one -- the same defect class as a silent scheduler
/// sleep, and no more acceptable here. `wait_s = 0` waits not at all. REFUSE never waits.
pub fn admit(
    requested_bytes: u64,
    tag: &str,
    ledger: Option<&Path>,
    poll_s: f64,
    wait_s: f64,
    announce: bool,
) -> Result<(Decision, Option<MemoryReservation>), String> {
    // NO DEADLINE ARITHMETIC ON AN UNBOUNDED NUMBER. `Duration::from_secs_f64` panics above
    // `u64::MAX` seconds and `Instant + Duration` panics on overflow, so the obvious
    // `Instant::now() + Duration::from_secs_f64(wait_s)` ABORTS the process (exit 101) on a wait
    // the caller was just told is acceptable -- worse than refusing it, because the run dies
    // without a verdict. Measure elapsed time against a clamped budget instead: no addition to an
    // Instant, no panic reachable, and the same ceiling the Python engine applies.
    let started = Instant::now();
    let budget = wait_budget(wait_s);
    let mut last_reason: Option<String> = None;
    loop {
        let (decision, reservation) = request(requested_bytes, tag, ledger)?;
        if decision.verdict != Verdict::Queue {
            if announce && last_reason.as_deref() != Some(decision.reason.as_str()) {
                println!("[admission] {}", decision.reason);
            }
            return Ok((decision, reservation));
        }
        if announce && last_reason.as_deref() != Some(decision.reason.as_str()) {
            println!("[admission] {}", decision.reason);
            last_reason = Some(decision.reason.clone());
        }
        let spent = started.elapsed();
        if spent >= budget {
            return Ok((decision, None));
        }
        let remaining = budget - spent;
        let poll = Duration::try_from_secs_f64(poll_s.max(0.01))
            .unwrap_or(Duration::from_secs(MAX_WAIT_SECONDS));
        std::thread::sleep(remaining.min(poll));
    }
}

/// Bytes THIS process already holds in the ledger, by `(pid, /proc starttime)`.
///
/// The identity, not a flag. `execvp` keeps both fields, so a runner that has re-exec'd into its
/// systemd scope can ask the ledger whether the reservation it is about to skip re-asking for is
/// genuinely its OWN. An environment variable cannot answer that: the scope exports the in-scope
/// sentinel to every process inside it, so a runner invoked as a STEP of a boxed run reads the
/// same "1" while holding nothing at all. Zero means "go and ask properly".
pub fn held_by_this_process(ledger: Option<&Path>) -> Result<u64, String> {
    let path = match ledger {
        Some(p) => p.to_path_buf(),
        None => default_ledger_path(),
    };
    let pid = std::process::id();
    let starttime = Some(current_starttime(pid)?);
    let _lock = LedgerLock::acquire(&path)?;
    let records = load(&path)?;
    Ok(records
        .iter()
        .filter(|r| r.pid == pid && r.starttime == starttime)
        .map(|r| r.bytes)
        .sum())
}

/// Total bytes currently reserved by LIVE holders. Sweeps dead holders first.
pub fn held_bytes(ledger: Option<&Path>) -> Result<u64, String> {
    let path = match ledger {
        Some(p) => p.to_path_buf(),
        None => default_ledger_path(),
    };
    let _lock = LedgerLock::acquire(&path)?;
    let records = load(&path)?;
    let (live, dead) = sweep(records)?;
    if !dead.is_empty() {
        store(&path, &live)?;
    }
    Ok(live.iter().map(|r| r.bytes).sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger_in(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "adm-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp ledger dir");
        dir.join("memory-admissions.json")
    }

    #[test]
    fn a_request_larger_than_the_host_budget_is_refused_and_names_the_number_to_ask_for() {
        // REFUSE rather than QUEUE is the whole point: no holder finishing makes the host bigger,
        // so a run told to wait here would wait forever for something that cannot happen.
        let path = ledger_in("refuse");
        let (decision, held) = request_with_limits(
            2_000_000,
            "t",
            Some(&path),
            Some(1_000_000),
            Some(1_000_000),
        )
        .expect("request");
        assert!(held.is_none());
        assert_eq!(decision.verdict, Verdict::Refuse);
        assert_eq!(decision.largest_grantable_bytes, Some(1_000_000));
        assert!(
            decision.reason.contains("1000000 bytes"),
            "a refusal must say the number to ask for: {:?}",
            decision.reason
        );
    }

    #[test]
    fn a_second_request_that_no_longer_fits_is_queued_behind_the_first() {
        let path = ledger_in("queue");
        let (first, held) =
            request_with_limits(700, "a", Some(&path), Some(1000), Some(1_000_000_000))
                .expect("first");
        assert_eq!(first.verdict, Verdict::Grant);
        let mut held = held.expect("granted");

        let (second, none) =
            request_with_limits(700, "b", Some(&path), Some(1000), Some(1_000_000_000))
                .expect("second");
        assert!(none.is_none());
        assert_eq!(second.verdict, Verdict::Queue);
        assert_eq!(second.reserved_bytes, 700);
        assert_eq!(
            second.holders_ahead, 1,
            "one holder releasing is enough, and the message must say so"
        );
        assert!(second.reason.contains("QUEUED on OTHER RUNS"));

        // Releasing the first must make the same request grant, or QUEUE was a lie.
        held.release().expect("release");
        let (third, granted) =
            request_with_limits(700, "b", Some(&path), Some(1000), Some(1_000_000_000))
                .expect("third");
        assert_eq!(third.verdict, Verdict::Grant, "{:?}", third.reason);
        assert!(granted.is_some());
    }

    #[test]
    fn host_memory_held_outside_the_tool_queues_on_a_named_different_cause() {
        // The ledger cannot see a non-runner tenant. Without the live reading, a host loaded to
        // 99% by something else still looks empty and admission grants into a swapping machine.
        let path = ledger_in("tenant");
        let (decision, held) =
            request_with_limits(500_000, "t", Some(&path), Some(1_000_000_000), Some(1000))
                .expect("request");
        assert!(held.is_none());
        assert_eq!(decision.verdict, Verdict::Queue);
        assert!(
            decision
                .reason
                .contains("HOST MEMORY held outside this tool"),
            "the two queue causes need different remedies, so they must read differently: {:?}",
            decision.reason
        );
        assert_eq!(
            decision.holders_ahead, 0,
            "no ledger holder is in the way, so promising that one will free it would be false"
        );
    }

    #[test]
    fn a_dead_holders_reservation_is_reclaimed_rather_than_subtracted_forever() {
        let path = ledger_in("dead");
        // A record whose holder cannot be alive. The (pid, starttime) fingerprint makes even a
        // recycled PID safe, so this can never free a live peer by accident.
        let ghost = Record {
            pid: u32::MAX,
            starttime: Some(1),
            bytes: 900,
            tag: "crashed".into(),
            ts: 1.0,
        };
        store(&path, &[ghost]).expect("store");
        let (decision, held) =
            request_with_limits(700, "t", Some(&path), Some(1000), Some(1_000_000_000))
                .expect("request");
        assert_eq!(
            decision.verdict,
            Verdict::Grant,
            "a crashed run must not subtract memory forever: {:?}",
            decision.reason
        );
        assert_eq!(decision.reserved_bytes, 0);
        assert!(held.is_some());
    }

    #[test]
    fn releasing_is_idempotent_and_does_not_free_a_peers_reservation() {
        let path = ledger_in("idem");
        let (_a, held_a) =
            request_with_limits(1000, "a", Some(&path), Some(10000), Some(1_000_000_000))
                .expect("a");
        let (_b, held_b) =
            request_with_limits(2000, "b", Some(&path), Some(10000), Some(1_000_000_000))
                .expect("b");
        let mut a = held_a.expect("a granted");
        let _b = held_b.expect("b granted");
        assert_eq!(held_bytes(Some(&path)).expect("held"), 3000);
        a.release().expect("release");
        a.release().expect("second release is a no-op");
        assert_eq!(
            held_bytes(Some(&path)).expect("held"),
            2000,
            "releasing twice must not take a peer's reservation with it"
        );
    }

    #[test]
    fn two_concurrent_requests_that_only_one_can_have_do_not_both_grant() {
        // The defect this module exists to remove, reproduced against the module itself: two
        // runs asking at the same instant must not both see the same free budget.
        let path = ledger_in("concurrent");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let granted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();
        for index in 0..8 {
            let path = path.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            let granted = std::sync::Arc::clone(&granted);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let (decision, held) = request_with_limits(
                    700,
                    &format!("t{index}"),
                    Some(&path),
                    Some(1000),
                    Some(1_000_000_000),
                )
                .expect("request");
                if decision.verdict == Verdict::Grant {
                    granted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // Hold it: releasing here would let a peer grant and hide the overcommit.
                    std::mem::forget(held);
                }
            }));
        }
        for handle in handles {
            handle.join().expect("thread");
        }
        assert_eq!(
            granted.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a 1000-byte budget can hold exactly one 700-byte reservation; more than one grant \
             means the lock did not cover the read-decide-record section"
        );
        assert_eq!(held_bytes(Some(&path)).expect("held"), 700);
    }

    #[test]
    fn the_flat_margin_never_swallows_a_small_hosts_entire_budget() {
        // 64 GiB and up: the cap is not binding and the whole flat margin applies.
        assert_eq!(safety_margin_bytes(64 * 1024 * 1024 * 1024), 8589934592);
        // 8 GiB: the uncapped 8 GiB margin would leave a budget of exactly zero, so every run
        // would be REFUSED and advised to "ask for at most 0 B". One eighth keeps 1 GiB back.
        assert_eq!(safety_margin_bytes(8 * 1024 * 1024 * 1024), 1073741824);
        assert_eq!(safety_margin_bytes(2 * 1024 * 1024 * 1024), 268435456);
        // What that buys: 85% of 8 GiB less 1 GiB is 5.8 GiB, a budget a real run fits in --
        // where the uncapped margin left 0 B and refused every run on the host.
        let small_budget = (8589934592.0_f64 * 0.85) as u64 - safety_margin_bytes(8589934592);
        assert_eq!(small_budget, 6227702579);
    }

    #[test]
    fn the_default_budget_keeps_back_the_named_fraction_and_the_flat_margin() {
        // THE FRACTION IS PINNED BY VALUE, HERE. Every other test in this module supplies the
        // budget explicitly (or through the operator override), so shrinking
        // DEFAULT_MEM_BUDGET_FRACTION used to leave the whole suite and the cross-language
        // differential green while the two engines disagreed by 9x about what a real host may
        // hold. The numbers below are literals, and they are the SAME literals the Python suite
        // pins, because the two editions share one ledger and must do one arithmetic.
        assert_eq!(DEFAULT_MEM_BUDGET_FRACTION, 0.85);
        // 85% of 64 GiB, less the flat 8 GiB margin.
        assert_eq!(budget_from_total(64 * 1024 * 1024 * 1024), 49821620633);
        // 85% of 8 GiB, less one eighth of 8 GiB rather than the whole 8 GiB.
        assert_eq!(budget_from_total(8 * 1024 * 1024 * 1024), 6227702579);
        assert_eq!(budget_from_total(0), 0);
        // Headroom keeps back the same margin, scaled by the HOST rather than by what is free.
        assert_eq!(
            headroom_from(8 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024),
            7516192768
        );
        assert_eq!(headroom_from(1024, 8 * 1024 * 1024 * 1024), 0);
    }

    #[test]
    fn this_process_can_recognise_its_own_reservation_in_the_ledger() {
        // What the in-scope skip is allowed to rely on. A run that re-execs into its systemd
        // scope keeps its pid and /proc start time, so its own record is findable; a runner
        // started as a STEP of that scope inherits the sentinel but has no record of its own,
        // and must be told so rather than waved through.
        let path = ledger_in("own-record");
        assert_eq!(held_by_this_process(Some(&path)).expect("empty"), 0);

        let (decision, reservation) =
            request_with_limits(4096, "run", Some(&path), Some(1 << 30), Some(1 << 30))
                .expect("request");
        assert_eq!(decision.verdict, Verdict::Grant);
        let mut reservation = reservation.expect("granted");
        assert_eq!(held_by_this_process(Some(&path)).expect("own"), 4096);

        // A live record belonging to SOMEONE ELSE is not this process's licence to skip: pid 1
        // is alive on every Linux host, and its bytes are counted by `held_bytes` and by nothing
        // this process may claim.
        let foreign =
            r#"{"admissions":[{"pid":1,"starttime":null,"bytes":8192,"tag":"run","ts":1.0}]}"#;
        fs::write(&path, foreign).expect("write foreign ledger");
        assert_eq!(held_bytes(Some(&path)).expect("held"), 8192);
        assert_eq!(held_by_this_process(Some(&path)).expect("foreign"), 0);
        let _ = reservation.release();
    }

    #[test]
    fn an_absurd_wait_is_clamped_instead_of_aborting_the_process() {
        // `Instant + Duration::from_secs_f64(1e19)` panics ("overflow when adding duration to
        // instant"), and `from_secs_f64(1e20)` panics before that ("cannot convert float seconds
        // to Duration"). Either way the run died with exit 101 on a wait value the CLI had just
        // validated. Granting must survive both, and must not sit here for a year.
        // A 4 EiB request is larger than any host, so the answer comes back without waiting --
        // but the deadline is built BEFORE the first request, which is exactly where the abort
        // happened. Nothing here may hang and nothing here may panic.
        let path = ledger_in("absurd-wait");
        for wait in [1e19_f64, 1e20_f64, f64::MAX] {
            let (decision, reservation) =
                admit(1 << 62, "run", Some(&path), 0.01, wait, false).expect("admit");
            assert_ne!(
                decision.verdict,
                Verdict::Queue,
                "a request bigger than any host is refused outright, not queued (wait_s={wait})"
            );
            if let Some(mut reservation) = reservation {
                let _ = reservation.release();
            }
        }
        // A year, named literally: the clamp is what keeps the deadline arithmetic in range, and
        // the Python edition folds the same three cases to the same three answers.
        assert_eq!(MAX_WAIT_SECONDS, 31536000);
        assert_eq!(wait_budget(1e19), Duration::from_secs(31536000));
        assert_eq!(wait_budget(1e20), Duration::from_secs(31536000));
        assert_eq!(wait_budget(f64::NAN), Duration::ZERO);
        assert_eq!(wait_budget(-5.0), Duration::ZERO);
        assert_eq!(wait_budget(30.5), Duration::from_secs_f64(30.5));
    }

    #[test]
    fn the_byte_rendering_matches_the_python_engine() {
        // The verdicts are compared across the two engines, so this formatting is a contract.
        assert_eq!(fmt_bytes(None), "unknown");
        assert_eq!(fmt_bytes(Some(0)), "0 B");
        assert_eq!(fmt_bytes(Some(1023)), "1023 B");
        assert_eq!(fmt_bytes(Some(1024)), "1.0 KiB");
        assert_eq!(fmt_bytes(Some(512 * 1024 * 1024)), "512.0 MiB");
        assert_eq!(fmt_bytes(Some(3 * 1024 * 1024 * 1024 / 2)), "1.5 GiB");
    }
}
