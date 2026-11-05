//! Cross-process enforcement for the existing named `resource_caps`.
//!
//! A scheduler process already prevents its own steps from exceeding the declared capacities.
//! When a run supplies a shared state path, this module applies the same counts across every
//! scheduler process using that path. Requests wait in ticket order, are granted atomically across
//! all resources a step needs, and are reclaimed after a process dies.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

/// Setting this path makes `resource_caps` apply across scheduler processes.
pub const PATH_ENV: &str = "DAGRUN_RESOURCE_CAPS_PATH";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestState {
    Waiting,
    Held,
}

impl RequestState {
    fn from_value(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "waiting" => Some(Self::Waiting),
            "held" => Some(Self::Held),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Held => "held",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Claim {
    demand: i64,
    capacity: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    pid: u32,
    starttime: u64,
    request: String,
    tag: String,
    ticket: u64,
    state: RequestState,
    claims: BTreeMap<String, Claim>,
}

impl Record {
    fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        let pid = value_u64(obj.get("pid")?)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)?;
        let starttime = value_u64(obj.get("starttime")?).filter(|value| *value > 0)?;
        let request = obj.get("request")?.as_str()?.to_string();
        let tag = obj.get("tag")?.as_str()?.to_string();
        let ticket = value_u64(obj.get("ticket")?)?;
        let state = RequestState::from_value(obj.get("state")?)?;
        if request.is_empty() || tag.is_empty() {
            return None;
        }
        let mut claims = BTreeMap::new();
        for (name, raw) in obj.get("resources")?.as_object()? {
            if name.is_empty() {
                return None;
            }
            let raw = raw.as_object()?;
            let demand = value_i64(raw.get("demand")?).filter(|value| *value > 0)?;
            let capacity = value_i64(raw.get("capacity")?).filter(|value| *value > 0)?;
            if demand > capacity {
                return None;
            }
            claims.insert(name.clone(), Claim { demand, capacity });
        }
        if claims.is_empty() {
            return None;
        }
        Some(Self {
            pid,
            starttime,
            request,
            tag,
            ticket,
            state,
            claims,
        })
    }

    fn to_value(&self) -> Value {
        let resources = self
            .claims
            .iter()
            .map(|(name, claim)| {
                (
                    name.clone(),
                    Value::Object(Map::from_iter([
                        ("demand".to_string(), Value::from(claim.demand)),
                        ("capacity".to_string(), Value::from(claim.capacity)),
                    ])),
                )
            })
            .collect();
        Value::Object(Map::from_iter([
            ("pid".to_string(), Value::from(self.pid)),
            ("starttime".to_string(), Value::from(self.starttime)),
            ("request".to_string(), Value::String(self.request.clone())),
            ("tag".to_string(), Value::String(self.tag.clone())),
            ("ticket".to_string(), Value::from(self.ticket)),
            (
                "state".to_string(),
                Value::String(self.state.as_str().to_string()),
            ),
            ("resources".to_string(), Value::Object(resources)),
        ]))
    }
}

#[derive(Debug, Default)]
struct Ledger {
    next_ticket: u64,
    requests: Vec<Record>,
}

fn value_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        _ => None,
    }
}

fn value_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

fn proc_starttime(pid: u32) -> Result<Option<u64>, String> {
    crate::procstat::read(pid)
        .map(|stat| stat.map(|stat| stat.starttime_ticks))
        .map_err(|error| error.to_string())
}

fn holder_alive(record: &Record) -> Result<bool, String> {
    Ok(proc_starttime(record.pid)?.is_some_and(|current| current == record.starttime))
}

struct LedgerLock {
    file: File,
}

impl LedgerLock {
    fn acquire(ledger: &Path) -> Result<Self, String> {
        let lock_path = PathBuf::from(format!("{}.lock", ledger.display()));
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "create resource_caps directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&lock_path)
            .map_err(|error| format!("open resource_caps lock {}: {error}", lock_path.display()))?;
        validate_private_regular(&file, &lock_path)?;
        // SAFETY: flock operates on this live file descriptor and retains no pointer.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "lock resource_caps state {}: {}",
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
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    // SAFETY: geteuid has no preconditions and does not access memory.
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_file() || metadata.uid() != uid || metadata.nlink() != 1 {
        return Err(format!(
            "{} is not an owned, single-link regular file",
            path.display()
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("make {} private: {error}", path.display()))?;
    }
    Ok(())
}

fn load(path: &Path) -> Result<Ledger, String> {
    let mut text = String::new();
    let mut file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "configured resource_caps state {} does not exist",
                path.display()
            ))
        }
        Err(error) => {
            return Err(format!(
                "open resource_caps state {}: {error}",
                path.display()
            ))
        }
    };
    validate_private_regular(&file, path)?;
    file.read_to_string(&mut text)
        .map_err(|error| format!("read resource_caps state {}: {error}", path.display()))?;
    let Value::Object(root) = serde_json::from_str::<Value>(&text)
        .map_err(|error| format!("resource_caps state {} is corrupt: {error}", path.display()))?
    else {
        return Err(format!(
            "resource_caps state {} root must be an object",
            path.display()
        ));
    };
    let next_ticket = value_u64(
        root.get("next_ticket")
            .ok_or_else(|| format!("resource_caps state {} has no next_ticket", path.display()))?,
    )
    .ok_or_else(|| {
        format!(
            "resource_caps state {} has an invalid next_ticket",
            path.display()
        )
    })?;
    let raw_requests = root
        .get("requests")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!(
                "resource_caps state {} has no requests list",
                path.display()
            )
        })?;
    let requests: Vec<Record> = raw_requests
        .iter()
        .map(|value| {
            Record::from_value(value).ok_or_else(|| {
                format!(
                    "resource_caps state {} has an invalid request",
                    path.display()
                )
            })
        })
        .collect::<Result<_, _>>()?;
    let mut request_ids = std::collections::HashSet::new();
    let mut tickets = std::collections::HashSet::new();
    if requests.iter().any(|record| {
        !request_ids.insert(record.request.clone())
            || !tickets.insert(record.ticket)
            || record.ticket >= next_ticket
    }) {
        return Err(format!(
            "resource_caps state {} has duplicate or out-of-range request identity",
            path.display()
        ));
    }
    Ok(Ledger {
        next_ticket,
        requests,
    })
}

fn store(path: &Path, ledger: &Ledger) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "create resource_caps directory {}: {error}",
            parent.display()
        )
    })?;
    let payload = Value::Object(Map::from_iter([
        ("next_ticket".to_string(), Value::from(ledger.next_ticket)),
        (
            "requests".to_string(),
            Value::Array(ledger.requests.iter().map(Record::to_value).collect()),
        ),
    ]));
    let bytes = serde_json::to_vec(&payload)
        .map_err(|error| format!("encode resource_caps state: {error}"))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut created = None;
    for attempt in 0..100u32 {
        let temp = parent.join(format!(
            ".resource-caps.{}.{}.{}.tmp",
            std::process::id(),
            stamp,
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
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create temporary resource_caps state: {error}")),
        }
    }
    let (temp, mut file) =
        created.ok_or_else(|| "could not allocate temporary resource_caps state".to_string())?;
    let result = (|| {
        file.write_all(&bytes)
            .map_err(|error| format!("write temporary resource_caps state: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("sync temporary resource_caps state: {error}"))?;
        drop(file);
        fs::rename(&temp, path).map_err(|error| format!("replace resource_caps state: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn sweep(ledger: &mut Ledger) -> Result<bool, String> {
    let before = ledger.requests.len();
    let mut retained = Vec::with_capacity(before);
    for record in std::mem::take(&mut ledger.requests) {
        if holder_alive(&record)? {
            retained.push(record);
        }
    }
    ledger.requests = retained;
    Ok(ledger.requests.len() != before)
}

fn claims_for(
    demands: &BTreeMap<String, i64>,
    capacities: &BTreeMap<String, i64>,
) -> Result<BTreeMap<String, Claim>, String> {
    demands
        .iter()
        .filter(|(_, demand)| **demand != 0)
        .map(|(name, demand)| {
            let capacity = capacities.get(name).copied().ok_or_else(|| {
                format!("resource {name:?} has demand {demand} but no declared resource_caps entry")
            })?;
            if *demand < 0 || capacity <= 0 || *demand > capacity {
                return Err(format!(
                    "resource {name:?} has demand {demand} and capacity {capacity}; both must be positive and demand must not exceed capacity"
                ));
            }
            Ok((
                name.clone(),
                Claim {
                    demand: *demand,
                    capacity,
                },
            ))
        })
        .collect()
}

fn capacity_conflict(ledger: &Ledger, claims: &BTreeMap<String, Claim>) -> Option<String> {
    for record in &ledger.requests {
        for (name, claim) in claims {
            if let Some(other) = record.claims.get(name) {
                if other.capacity != claim.capacity {
                    return Some(format!(
                        "resource {name:?} has conflicting capacities {} and {}",
                        claim.capacity, other.capacity
                    ));
                }
            }
        }
    }
    None
}

/// One request in the shared `resource_caps` state.
#[derive(Debug)]
pub struct Reservation {
    path: PathBuf,
    request: String,
    claims: BTreeMap<String, Claim>,
    queued_at: Instant,
    held: bool,
    released: bool,
}

impl Reservation {
    fn create(
        path: &Path,
        tag: &str,
        demands: &BTreeMap<String, i64>,
        capacities: &BTreeMap<String, i64>,
    ) -> Result<Self, String> {
        let claims = claims_for(demands, capacities)?;
        let _lock = LedgerLock::acquire(path)?;
        let mut ledger = load(path)?;
        sweep(&mut ledger)?;
        if let Some(error) = capacity_conflict(&ledger, &claims) {
            return Err(error);
        }
        let pid = std::process::id();
        let starttime = proc_starttime(pid)?.ok_or_else(|| {
            format!("could not read current process identity from /proc/{pid}/stat")
        })?;
        let ticket = ledger.next_ticket;
        ledger.next_ticket = ledger
            .next_ticket
            .checked_add(1)
            .ok_or_else(|| "resource_caps ticket counter overflowed".to_string())?;
        let request = format!("{pid}-{starttime}-{ticket}");
        ledger.requests.push(Record {
            pid,
            starttime,
            request: request.clone(),
            tag: tag.to_string(),
            ticket,
            state: RequestState::Waiting,
            claims: claims.clone(),
        });
        store(path, &ledger)?;
        Ok(Self {
            path: path.to_path_buf(),
            request,
            claims,
            queued_at: Instant::now(),
            held: false,
            released: false,
        })
    }

    fn try_grant(&mut self) -> Result<bool, String> {
        if self.held {
            return Ok(true);
        }
        let _lock = LedgerLock::acquire(&self.path)?;
        let mut ledger = load(&self.path)?;
        let changed = sweep(&mut ledger)?;
        let index = ledger
            .requests
            .iter()
            .position(|record| record.request == self.request)
            .ok_or_else(|| {
                format!(
                    "resource_caps request {} disappeared from {}",
                    self.request,
                    self.path.display()
                )
            })?;
        if ledger.requests[index].claims != self.claims {
            return Err(format!(
                "resource_caps request {} changed while waiting",
                self.request
            ));
        }
        if let Some(error) = capacity_conflict(&ledger, &self.claims) {
            return Err(error);
        }
        let ticket = ledger.requests[index].ticket;
        let blocked_by_earlier = ledger.requests.iter().any(|record| {
            record.state == RequestState::Waiting
                && record.ticket < ticket
                && record
                    .claims
                    .keys()
                    .any(|name| self.claims.contains_key(name))
        });
        let mut held: HashMap<&str, i64> = HashMap::new();
        for record in ledger
            .requests
            .iter()
            .filter(|record| record.state == RequestState::Held)
        {
            for (name, claim) in &record.claims {
                *held.entry(name.as_str()).or_insert(0) += claim.demand;
            }
        }
        let fits = self.claims.iter().all(|(name, claim)| {
            held.get(name.as_str()).copied().unwrap_or(0) + claim.demand <= claim.capacity
        });
        if !blocked_by_earlier && fits {
            ledger.requests[index].state = RequestState::Held;
            store(&self.path, &ledger)?;
            self.held = true;
            return Ok(true);
        }
        if changed {
            store(&self.path, &ledger)?;
        }
        Ok(false)
    }

    /// Time spent waiting before the step itself started.
    pub fn waited_seconds(&self) -> f64 {
        self.queued_at.elapsed().as_secs_f64()
    }

    fn release(&mut self) -> Result<(), String> {
        if self.released {
            return Ok(());
        }
        let _lock = LedgerLock::acquire(&self.path)?;
        let mut ledger = load(&self.path)?;
        sweep(&mut ledger)?;
        ledger
            .requests
            .retain(|record| record.request != self.request);
        store(&self.path, &ledger)?;
        self.released = true;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            eprintln!(
                "[scheduler] WARNING: could not release resource_caps request {}: {error}",
                self.request
            );
        }
    }
}

/// Result of checking whether one step can enter the existing `resource_caps`.
pub enum Acquire {
    /// The request remains queued; `newly_queued` distinguishes the first observable wait.
    Waiting {
        /// True only for the call that first wrote this request to the shared state.
        newly_queued: bool,
    },
    /// The request now holds every declared resource until `reservation` is dropped.
    Granted {
        /// Durable reservation whose lifetime brackets the step process.
        reservation: Reservation,
        /// Wall time spent queued before the step's own timer starts.
        waited_seconds: f64,
    },
}

/// Per-run client for the shared `resource_caps` path.
pub struct Coordinator {
    path: PathBuf,
    capacities: BTreeMap<String, i64>,
    pending: Mutex<HashMap<String, Reservation>>,
}

impl Coordinator {
    /// Enable cross-process enforcement only when the caller supplies the shared path.
    pub fn from_env(capacities: &BTreeMap<String, i64>) -> Result<Option<Self>, String> {
        let Some(path) = std::env::var_os(PATH_ENV).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        Self::new(PathBuf::from(path), capacities.clone()).map(Some)
    }

    /// Construct a coordinator over an explicit path.
    pub fn new(path: PathBuf, capacities: BTreeMap<String, i64>) -> Result<Self, String> {
        if !path.exists() && !path.is_symlink() {
            return Err(format!(
                "configured resource_caps state {} does not exist",
                path.display()
            ));
        }
        for (name, capacity) in &capacities {
            if name.is_empty() || *capacity < 0 {
                return Err(format!(
                    "resource_caps entry {name:?} has invalid capacity {capacity}"
                ));
            }
        }
        let _lock = LedgerLock::acquire(&path)?;
        let mut ledger = load(&path)?;
        if sweep(&mut ledger)? {
            store(&path, &ledger)?;
        }
        for record in &ledger.requests {
            for (name, capacity) in &capacities {
                if let Some(other) = record.claims.get(name) {
                    if other.capacity != *capacity {
                        return Err(format!(
                            "resource {name:?} has conflicting capacities {capacity} and {}",
                            other.capacity
                        ));
                    }
                }
            }
        }
        Ok(Self {
            path,
            capacities,
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// The configured shared state path, for startup disclosure.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Try to admit one step without starting its wall timer while it waits.
    pub fn try_acquire(
        &self,
        tag: &str,
        demands: &BTreeMap<String, i64>,
    ) -> Result<Acquire, String> {
        if demands.is_empty() {
            return Err("try_acquire called for a step with no resource demand".to_string());
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut newly_queued = false;
        if !pending.contains_key(tag) {
            for (name, demand) in demands {
                let waiting: i64 = pending
                    .values()
                    .filter_map(|request| request.claims.get(name))
                    .map(|claim| claim.demand)
                    .sum();
                let capacity = self.capacities.get(name).copied().unwrap_or(0);
                if waiting + demand > capacity {
                    return Ok(Acquire::Waiting {
                        newly_queued: false,
                    });
                }
            }
            let request = Reservation::create(&self.path, tag, demands, &self.capacities)?;
            pending.insert(tag.to_string(), request);
            newly_queued = true;
        }
        let granted = pending
            .get_mut(tag)
            .ok_or_else(|| format!("resource_caps request for step {tag:?} disappeared"))?
            .try_grant()?;
        if !granted {
            return Ok(Acquire::Waiting { newly_queued });
        }
        let reservation = pending
            .remove(tag)
            .ok_or_else(|| format!("resource_caps grant for step {tag:?} disappeared"))?;
        let waited_seconds = reservation.waited_seconds();
        Ok(Acquire::Granted {
            reservation,
            waited_seconds,
        })
    }

    /// Whether this run has a request waiting in the shared state.
    pub fn has_pending(&self) -> bool {
        !self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    }

    /// Remove every request that never reached a step process.
    pub fn clear_pending(&self) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Remove requests for steps that can no longer be launched by this run.
    pub fn retain_pending(&self, eligible: &std::collections::HashSet<String>) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|tag, _| eligible.contains(tag));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dagrun-resource-caps-{}-{name}.json",
            std::process::id()
        ))
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(format!("{}.lock", path.display()));
    }

    fn initialize(path: &Path) {
        store(path, &Ledger::default()).unwrap();
    }

    #[test]
    fn missing_configured_state_is_refused() {
        let path = temp_path("missing");
        cleanup(&path);
        let error = Coordinator::new(path.clone(), BTreeMap::from([("guest".into(), 1)]))
            .err()
            .expect("a configured missing file must be refused");
        assert!(error.contains("does not exist"), "{error}");
        assert!(!path.exists());
        cleanup(&path);
    }

    #[test]
    fn second_request_waits_until_the_first_releases() {
        let path = temp_path("wait");
        cleanup(&path);
        initialize(&path);
        let caps = BTreeMap::from([("guest".to_string(), 1)]);
        let first = Coordinator::new(path.clone(), caps.clone()).unwrap();
        let second = Coordinator::new(path.clone(), caps).unwrap();
        let demand = BTreeMap::from([("guest".to_string(), 1)]);

        let Acquire::Granted {
            reservation: held, ..
        } = first.try_acquire("a", &demand).unwrap()
        else {
            panic!("first request was not granted");
        };
        assert!(matches!(
            second.try_acquire("b", &demand).unwrap(),
            Acquire::Waiting { .. }
        ));
        drop(held);
        assert!(matches!(
            second.try_acquire("b", &demand).unwrap(),
            Acquire::Granted { .. }
        ));
        cleanup(&path);
    }

    #[test]
    fn malformed_state_is_refused() {
        let path = temp_path("malformed");
        cleanup(&path);
        fs::write(&path, b"{not json\n").unwrap();
        let error = Coordinator::new(path.clone(), BTreeMap::from([("guest".into(), 1)]))
            .err()
            .expect("malformed shared state must be refused");
        assert!(error.contains("is corrupt"), "{error}");
        cleanup(&path);
    }

    #[test]
    fn state_without_process_starttime_is_refused() {
        let path = temp_path("missing-process-starttime");
        cleanup(&path);
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "next_ticket": 1,
                "requests": [{
                    "pid": std::process::id(),
                    "starttime": null,
                    "request": "missing-identity",
                    "tag": "g.step",
                    "ticket": 0,
                    "state": "held",
                    "resources": {"guest": {"demand": 1, "capacity": 1}}
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let error = Coordinator::new(path.clone(), BTreeMap::from([("guest".into(), 1)]))
            .err()
            .expect("state without a reusable-process guard must be refused");
        assert!(error.contains("invalid request"), "{error}");
        cleanup(&path);
    }

    #[test]
    fn request_from_exited_process_is_reclaimed() {
        let path = temp_path("exited-process");
        cleanup(&path);
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = child.id();
        let starttime = proc_starttime(pid).unwrap().unwrap();
        store(
            &path,
            &Ledger {
                next_ticket: 1,
                requests: vec![Record {
                    pid,
                    starttime,
                    request: format!("{pid}-{starttime}-0"),
                    tag: "g.exited".to_string(),
                    ticket: 0,
                    state: RequestState::Held,
                    claims: BTreeMap::from([(
                        "guest".to_string(),
                        Claim {
                            demand: 1,
                            capacity: 1,
                        },
                    )]),
                }],
            },
        )
        .unwrap();
        child.kill().unwrap();
        child.wait().unwrap();

        Coordinator::new(path.clone(), BTreeMap::from([("guest".into(), 1)])).unwrap();
        assert!(load(&path).unwrap().requests.is_empty());
        cleanup(&path);
    }

    #[test]
    fn conflicting_live_capacities_are_refused() {
        let path = temp_path("capacity");
        cleanup(&path);
        initialize(&path);
        let one = Coordinator::new(path.clone(), BTreeMap::from([("guest".into(), 1)])).unwrap();
        let demand = BTreeMap::from([("guest".into(), 1)]);
        let Acquire::Granted { reservation, .. } = one.try_acquire("a", &demand).unwrap() else {
            panic!("first request was not granted");
        };
        let error = Coordinator::new(path.clone(), BTreeMap::from([("guest".into(), 2)]))
            .err()
            .expect("conflicting capacity must be refused");
        assert!(error.contains("conflicting capacities"), "{error}");
        drop(reservation);
        cleanup(&path);
    }

    #[test]
    fn capacity_counts_apply_across_requests_not_only_at_one() {
        let path = temp_path("counted-capacity");
        cleanup(&path);
        initialize(&path);
        let caps = BTreeMap::from([("guest".to_string(), 8)]);
        let first = Coordinator::new(path.clone(), caps.clone()).unwrap();
        let second = Coordinator::new(path.clone(), caps.clone()).unwrap();
        let third = Coordinator::new(path.clone(), caps).unwrap();
        let four = BTreeMap::from([("guest".to_string(), 4)]);
        let one = BTreeMap::from([("guest".to_string(), 1)]);

        let Acquire::Granted {
            reservation: first_four,
            ..
        } = first.try_acquire("first-four", &four).unwrap()
        else {
            panic!("first four units were not granted");
        };
        let Acquire::Granted {
            reservation: second_four,
            ..
        } = second.try_acquire("second-four", &four).unwrap()
        else {
            panic!("second four units were not granted");
        };
        assert!(matches!(
            third.try_acquire("ninth", &one).unwrap(),
            Acquire::Waiting { .. }
        ));
        drop(first_four);
        assert!(matches!(
            third.try_acquire("ninth", &one).unwrap(),
            Acquire::Granted { .. }
        ));
        drop(second_four);
        cleanup(&path);
    }

    #[test]
    fn zero_demand_needs_no_shared_request() {
        let claims = claims_for(
            &BTreeMap::from([("guest".to_string(), 0)]),
            &BTreeMap::from([("guest".to_string(), 0)]),
        )
        .unwrap();
        assert!(claims.is_empty());
    }
}
