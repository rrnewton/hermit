//! Durable, collision-free CPU-core reservations shared by local processes.

// Durable, cross-process CPU-core reservations.
//
// The ledger is shared with the Python distribution: both implementations lock the same sibling
// `.lock` file, read/write the same JSON schema, fingerprint holders by `(pid, /proc starttime)`,
// reclaim dead holders, and select only from free allowed CPUs.  A reservation is released on
// `Drop`; a crash is recovered by the next ledger operation.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Number, Value};

use crate::cgroup::pick_least_busy_free_cores_excluding;

/// Environment variable overriding the host-wide reservation ledger.
pub const LEDGER_ENV: &str = "DAGRUN_CORE_LEDGER";

#[derive(Clone, Debug, PartialEq)]
struct Record {
    pid: u32,
    starttime: Option<u64>,
    cores: Vec<usize>,
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
        let raw_cores = obj.get("cores")?.as_array()?;
        if raw_cores.is_empty() {
            return None;
        }
        let cores: Vec<usize> = raw_cores
            .iter()
            .map(|value| {
                let core = value_u64(value)?;
                if core > u64::from(u32::MAX) {
                    return None;
                }
                usize::try_from(core).ok()
            })
            .collect::<Option<_>>()?;
        if cores.iter().copied().collect::<HashSet<_>>().len() != cores.len() {
            return None;
        }
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
            cores,
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
        obj.insert(
            "cores".into(),
            Value::Array(self.cores.iter().copied().map(Value::from).collect()),
        );
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

/// Resolve the host-wide reservation-ledger path from the environment or runtime directory.
pub fn default_ledger_path() -> PathBuf {
    if let Some(path) = std::env::var_os(LEDGER_ENV) {
        return PathBuf::from(path);
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let runtime = PathBuf::from(runtime);
        if runtime.is_dir() {
            return runtime.join("dagrun").join("core-reservations.json");
        }
    }
    // SAFETY: geteuid has no preconditions and does not access memory.
    let uid = unsafe { libc::geteuid() };
    std::env::temp_dir()
        .join(format!("dagrun-{uid}"))
        .join("core-reservations.json")
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
            .map_err(|e| format!("open ledger lock {}: {e}", lock_path.display()))?;
        validate_private_regular(&file, &lock_path)?;
        // SAFETY: flock operates on this live file descriptor and does not retain a pointer.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "lock reservation ledger {}: {}",
                lock_path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { file })
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

impl Drop for LedgerLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid until `file` is dropped after this method.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
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
        Err(error) => {
            return Err(format!(
                "open reservation ledger {}: {error}",
                path.display()
            ))
        }
    };
    validate_private_regular(&file, path)?;
    file.read_to_string(&mut text)
        .map_err(|e| format!("read reservation ledger {}: {e}", path.display()))?;
    let Value::Object(root) = serde_json::from_str::<Value>(&text)
        .map_err(|e| format!("reservation ledger {} is corrupt: {e}", path.display()))?
    else {
        return Err(format!(
            "reservation ledger {} root must be an object",
            path.display()
        ));
    };
    let items = root
        .get("reservations")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!(
                "reservation ledger {} has no reservations list",
                path.display()
            )
        })?;
    items
        .iter()
        .map(|item| {
            Record::from_value(item).ok_or_else(|| {
                format!(
                    "reservation ledger {} has an invalid record",
                    path.display()
                )
            })
        })
        .collect()
}

fn store(path: &Path, records: &[Record]) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| format!("create ledger directory: {e}"))?;
    let payload = Value::Object(Map::from_iter([(
        "reservations".to_string(),
        Value::Array(records.iter().map(Record::to_value).collect()),
    )]));
    let bytes = serde_json::to_vec(&payload).map_err(|e| format!("encode ledger: {e}"))?;
    let mut created: Option<(PathBuf, File)> = None;
    for attempt in 0..100u32 {
        let temp = parent.join(format!(
            ".core-reservations.{}.{}.tmp",
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
        fs::rename(&temp, path).map_err(|e| format!("replace reservation ledger: {e}"))
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

/// A collision-free core reservation.  Dropping it releases its ledger record.
#[derive(Debug)]
pub struct Reservation {
    /// Exact reserved CPU IDs.
    pub cores: Vec<usize>,
    pid: u32,
    starttime: Option<u64>,
    tag: String,
    ledger: PathBuf,
    released: bool,
}

impl Reservation {
    /// Release this reservation.  Idempotent.
    pub fn release(&mut self) -> Result<(), String> {
        if self.released {
            return Ok(());
        }
        let _lock = LedgerLock::acquire(&self.ledger)?;
        let kept: Vec<Record> = load(&self.ledger)?
            .into_iter()
            .filter(|r| {
                !(r.pid == self.pid
                    && r.starttime == self.starttime
                    && r.tag == self.tag
                    && r.cores == self.cores)
            })
            .collect();
        store(&self.ledger, &kept)?;
        self.released = true;
        Ok(())
    }

    /// Path of the ledger holding this reservation.
    pub fn ledger(&self) -> &Path {
        &self.ledger
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// Reserve `k` disjoint, least-busy cores for this process.
pub fn acquire(
    k: i64,
    tag: &str,
    sample_s: f64,
    ledger: Option<&Path>,
    exclude: &HashSet<usize>,
    max_irq_rate: Option<f64>,
) -> Result<Reservation, String> {
    if k < 1 {
        return Err(format!("k must be >= 1, got {k}"));
    }
    if !sample_s.is_finite() || sample_s < 0.0 {
        return Err(format!("sample_s must be finite and >= 0, got {sample_s}"));
    }
    if let Some(limit) = max_irq_rate {
        if !limit.is_finite() || limit < 0.0 {
            return Err(format!("max_irq_rate must be finite and >= 0, got {limit}"));
        }
        if sample_s <= 0.0 {
            return Err("sample_s must be > 0 when max_irq_rate is set".to_string());
        }
    }
    let path = ledger
        .map(Path::to_path_buf)
        .unwrap_or_else(default_ledger_path);
    let pid = std::process::id();
    let starttime = Some(current_starttime(pid)?);
    let _lock = LedgerLock::acquire(&path)?;
    let (live, _) = sweep(load(&path)?)?;
    let mut held = exclude.clone();
    for record in &live {
        held.extend(record.cores.iter().copied());
    }
    let cores = pick_least_busy_free_cores_excluding(k, sample_s, &held, max_irq_rate);
    if cores.len() < k as usize {
        store(&path, &live)?;
        return Err(format!(
            "requested {k} core(s) but only {} free-and-unheld (held by {} live reservation(s): {:?}; allowed set may also be smaller)",
            cores.len(),
            live.len(),
            {
                let mut values: Vec<usize> = held.into_iter().collect();
                values.sort_unstable();
                values
            }
        ));
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut updated = live;
    updated.push(Record {
        pid,
        starttime,
        cores: cores.clone(),
        tag: tag.to_string(),
        ts,
    });
    store(&path, &updated)?;
    Ok(Reservation {
        cores,
        pid,
        starttime,
        tag: tag.to_string(),
        ledger: path,
        released: false,
    })
}

/// Reclaim and return dead-holder records.
pub fn reclaim_dead(ledger: Option<&Path>) -> Result<Vec<Value>, String> {
    let path = ledger
        .map(Path::to_path_buf)
        .unwrap_or_else(default_ledger_path);
    let _lock = LedgerLock::acquire(&path)?;
    let (live, dead) = sweep(load(&path)?)?;
    if !dead.is_empty() {
        store(&path, &live)?;
    }
    Ok(dead.iter().map(Record::to_value).collect())
}

/// Return all currently held cores after reclaiming dead holders.
pub fn held_cores(ledger: Option<&Path>) -> Result<Vec<usize>, String> {
    let path = ledger
        .map(Path::to_path_buf)
        .unwrap_or_else(default_ledger_path);
    let _lock = LedgerLock::acquire(&path)?;
    let (live, dead) = sweep(load(&path)?)?;
    if !dead.is_empty() {
        store(&path, &live)?;
    }
    let mut cores: Vec<usize> = live
        .iter()
        .flat_map(|record| record.cores.iter().copied())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    cores.sort_unstable();
    Ok(cores)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    fn fifo(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: path is a live NUL-terminated string for this call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    fn temp_ledger(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dagrun-rs-reservation-{}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn acquire_release_and_shared_schema() {
        let path = temp_ledger("release");
        let _ = fs::remove_file(&path);
        let mut reservation =
            acquire(1, "unit", 0.001, Some(&path), &HashSet::new(), None).unwrap();
        assert_eq!(held_cores(Some(&path)).unwrap(), reservation.cores);
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed["reservations"][0]["starttime"].is_number());
        reservation.release().unwrap();
        assert!(held_cores(Some(&path)).unwrap().is_empty());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(format!("{}.lock", path.display()));
    }

    #[test]
    fn acquire_rejects_invalid_sample_windows() {
        let path = temp_ledger("sample-window");
        for sample_s in [-1.0, f64::NAN, f64::INFINITY] {
            let error = acquire(1, "unit", sample_s, Some(&path), &HashSet::new(), None)
                .expect_err("invalid sample window must be rejected");
            assert!(error.contains("sample_s must be finite and >= 0"));
        }
        let error = acquire(1, "unit", 0.0, Some(&path), &HashSet::new(), Some(10.0))
            .expect_err("an IRQ budget needs a nonzero measurement window");
        assert!(error.contains("sample_s must be > 0"));
    }

    #[test]
    fn corrupt_and_special_ledgers_fail_closed() {
        let corrupt = temp_ledger("corrupt");
        let corrupt_lock = PathBuf::from(format!("{}.lock", corrupt.display()));
        let _ = fs::remove_file(&corrupt);
        let _ = fs::remove_file(&corrupt_lock);
        fs::write(&corrupt, "{").unwrap();
        fs::set_permissions(&corrupt, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(held_cores(Some(&corrupt))
            .expect_err("corrupt state must fail")
            .contains("corrupt"));

        let fifo_ledger = temp_ledger("fifo-ledger");
        let fifo_ledger_lock = PathBuf::from(format!("{}.lock", fifo_ledger.display()));
        let _ = fs::remove_file(&fifo_ledger);
        let _ = fs::remove_file(&fifo_ledger_lock);
        fifo(&fifo_ledger);
        assert!(held_cores(Some(&fifo_ledger)).is_err());

        let fifo_lock_ledger = temp_ledger("fifo-lock");
        let fifo_lock = PathBuf::from(format!("{}.lock", fifo_lock_ledger.display()));
        let _ = fs::remove_file(&fifo_lock_ledger);
        let _ = fs::remove_file(&fifo_lock);
        fifo(&fifo_lock);
        assert!(held_cores(Some(&fifo_lock_ledger)).is_err());

        let _ = fs::remove_file(corrupt);
        let _ = fs::remove_file(corrupt_lock);
        let _ = fs::remove_file(fifo_ledger);
        let _ = fs::remove_file(fifo_ledger_lock);
        let _ = fs::remove_file(fifo_lock);
    }

    #[test]
    fn invalid_record_schema_is_rejected_without_coercion() {
        let valid = serde_json::json!({
            "pid": 1,
            "starttime": 1,
            "cores": [0],
            "tag": "holder",
            "ts": 1.0,
        });
        assert!(Record::from_value(&valid).is_some());

        let invalid = [
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [0, "bad"], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 1, "starttime": 1, "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [-1], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [4294967296_u64], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [true], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [1.0], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [1, 1], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 0, "starttime": 1, "cores": [0], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 4294967296_u64, "starttime": 1, "cores": [0], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": true, "starttime": 1, "cores": [0], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"starttime": 1, "cores": [0], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 1, "cores": [0], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 1, "starttime": 0, "cores": [0], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 1, "starttime": "1", "cores": [0], "tag": "holder", "ts": 1.0}),
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [0], "tag": 7, "ts": 1.0}),
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [0], "ts": 1.0}),
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [0], "tag": "holder"}),
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [0], "tag": "holder", "ts": "1.0"}),
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [0], "tag": "holder", "ts": true}),
            serde_json::json!({"pid": 1, "starttime": 1, "cores": [0], "tag": "holder", "ts": -1}),
        ];
        assert!(invalid
            .iter()
            .all(|value| Record::from_value(value).is_none()));

        let path = temp_ledger("invalid-schema-preserved");
        let lock = PathBuf::from(format!("{}.lock", path.display()));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&lock);
        let original = r#"{"reservations":[{"pid":1,"starttime":1,"cores":[0,"bad"],"tag":"holder","ts":1.0}]}"#;
        fs::write(&path, original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(held_cores(Some(&path)).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(lock);
    }

    #[test]
    fn dead_holder_is_reclaimed() {
        let path = temp_ledger("dead");
        let dead = Record {
            pid: u32::MAX,
            starttime: Some(1),
            cores: vec![0],
            tag: "dead".into(),
            ts: 0.0,
        };
        store(&path, &[dead]).unwrap();
        let reclaimed = reclaim_dead(Some(&path)).unwrap();
        assert_eq!(reclaimed.len(), 1);
        assert!(held_cores(Some(&path)).unwrap().is_empty());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(format!("{}.lock", path.display()));
    }

    #[test]
    fn releasing_one_same_tag_reservation_keeps_the_other() {
        if crate::cgroup::pick_least_busy_free_cores(2, 0.0).len() < 2 {
            return;
        }
        let path = temp_ledger("same-tag");
        let _ = fs::remove_file(&path);
        let mut first = acquire(1, "same", 0.001, Some(&path), &HashSet::new(), None).unwrap();
        let mut second = acquire(1, "same", 0.001, Some(&path), &HashSet::new(), None).unwrap();
        assert_ne!(first.cores, second.cores);
        first.release().unwrap();
        assert_eq!(held_cores(Some(&path)).unwrap(), second.cores);
        second.release().unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(format!("{}.lock", path.display()));
    }
}
