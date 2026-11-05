//! Direct typed access to Herdr's public CLI for interactive agent control.
//!
//! Herdr owns protocol negotiation and terminal processes. This adapter starts no
//! server and has no shell executor, broker, or allowlist dependency.
use crate::error::{AdapterError, Result};
use chat_subscription_plugin::process::ProcessPluginChild;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_STDOUT_BYTES: usize = 8 * 1024 * 1024;
const CONTROL_STDERR_BYTES: usize = 64 * 1024;
const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const CAPTURE_READ_BURST: usize = 256 * 1024;
const PROC_STAT_BYTES: usize = 8 * 1024;
const BOOT_ID_BYTES: usize = 128;
const SUPPORTED_PANE_SHELLS: [&str; 6] = ["bash", "zsh", "sh", "dash", "fish", "ksh"];
const MAX_SHELL_TASKS: usize = 4096;
const MAX_CHILDREN_BYTES: usize = 64 * 1024;
const MAX_PROC_ENTRIES: usize = 1024 * 1024;
#[cfg(all(test, target_os = "linux"))]
static POLL_CAPTURE_INTERRUPTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Durable identity of one pane-owned process generation.
///
/// A pathname is deliberately absent: package upgrades may atomically replace the
/// installed image while the old process remains alive. The boot/start tuple binds
/// the PID, while device/inode binds the executable image that Linux actually ran.
/// Each observation holds a pidfd so PID reuse cannot interleave its `/proc` reads.
/// Across separate invocations, Linux exposes no persistent handle in this schema;
/// reuse of the same PID by the same executable within one scheduler tick remains a
/// theoretical ambiguity. The identity is used only alongside pane and terminal-state
/// checks; it never independently authorizes a signal to the PID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustomProcessIdentity {
    /// Identity schema version.
    pub version: u32,
    /// Canonical Linux boot UUID.
    pub boot_id: String,
    /// Positive Linux process identifier.
    pub pid: u64,
    /// Field 22 from `/proc/PID/stat`.
    pub starttime_ticks: u64,
    /// Device number of the opened executable image.
    pub executable_device: u64,
    /// Inode number of the opened executable image.
    pub executable_inode: u64,
}

/// Whether a recorded local process can still be the same kernel generation.
///
/// This classification never independently authorizes input or a signal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessLiveness {
    /// A coherent process generation and executable image match the record.
    Alive,
    /// The kernel positively establishes that the recorded generation is gone.
    Dead,
    /// The probe failed, changed during observation, or found a different live image.
    Unknown,
}

/// Exact supported idle-shell process generation and kernel executable path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneShellProof {
    /// Kernel process/image generation.
    pub identity: CustomProcessIdentity,
    /// Kernel-reported executable path observed with that generation.
    pub executable_path: PathBuf,
}

impl CustomProcessIdentity {
    pub(crate) fn valid(&self) -> bool {
        self.version == 1
            && canonical_boot_uuid(&self.boot_id)
            && (1..=i32::MAX as u64).contains(&self.pid)
            && self.starttime_ticks > 0
            && self.executable_device > 0
            && self.executable_inode > 0
    }
}

#[derive(Debug)]
struct PinnedHarnessExecutable {
    path: PathBuf,
    _image: File,
    device: u64,
    inode: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct LiveCustomProcess {
    identity: CustomProcessIdentity,
    process_group_id: u64,
    executable_path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PaneProcessState {
    shell_pid: u64,
    foreground_process_group_id: u64,
    processes: Vec<Map<String, Value>>,
}

fn foreground_argv0(process: &Map<String, Value>) -> Result<Option<&str>> {
    let values = match process.get("argv") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(values)) => values,
        Some(_) => {
            return Err(AdapterError::unavailable(
                "foreground process argv: expected an array",
            ))
        }
    };
    if values.iter().any(|value| !value.is_string()) {
        return Err(AdapterError::unavailable(
            "foreground process argv: expected string arguments",
        ));
    }
    let Some(value) = values.first().and_then(Value::as_str) else {
        return Ok(None);
    };
    if value.is_empty() || value.contains('\0') {
        return Err(AdapterError::unavailable(
            "foreground process argv must have a nonempty argv[0] without NUL",
        ));
    }
    Ok(Some(value))
}

fn foreground_process_entries(values: &[Value]) -> Result<Vec<Map<String, Value>>> {
    let processes = object_entries(values, "foreground process")?;
    for process in &processes {
        foreground_argv0(process)?;
    }
    Ok(processes)
}

/// Whether a Claude or Codex workspace trust dialog is on screen.
pub fn relay_trust_prompt(screen: &str) -> bool {
    (screen.contains("Quick safety check: Is this a project you created or one you trust?")
        && screen.contains("Yes, I trust this folder"))
        || (screen.contains("Trust this folder?") && screen.contains("Trust and continue"))
}

/// Parent -> children for every visible process, from world-readable stat files. A root-owned
/// process's own task directory is not readable by the user, so descendants of sudo are found
/// through their parent links instead.
fn process_children_map() -> HashMap<u64, Vec<u64>> {
    let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return children;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u64>().ok())
        else {
            continue;
        };
        let Ok(value) = bounded_file(&entry.path().join("stat"), PROC_STAT_BYTES, "process stat")
        else {
            continue;
        };
        if let Ok(stat) = parse_process_stat(&value, pid) {
            children.entry(stat.parent_pid).or_default().push(pid);
        }
    }
    children
}

/// The pane's own shell PID is the sole foreground process, is a shell, and sits in a
/// wrkslots slice.
fn slot_boxed_shell(state: &PaneProcessState) -> bool {
    const SHELLS: [&str; 6] = ["bash", "zsh", "sh", "fish", "dash", "ksh"];
    let only_shell = !state.processes.is_empty()
        && state
            .processes
            .iter()
            .all(|process| process.get("pid").and_then(Value::as_u64) == Some(state.shell_pid));
    let is_shell = state.processes.iter().any(|process| {
        process
            .get("argv")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
            .and_then(Value::as_str)
            .map(|argv0| {
                argv0
                    .rsplit('/')
                    .next()
                    .unwrap_or(argv0)
                    .trim_start_matches('-')
            })
            .is_some_and(|name| SHELLS.contains(&name))
    });
    only_shell
        && is_shell
        && fs::read_to_string(format!("/proc/{}/cgroup", state.shell_pid))
            .is_ok_and(|cgroup| cgroup.contains("wrkslots"))
}

fn canonical_boot_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

fn bounded_file(path: &Path, limit: usize, label: &str) -> Result<Vec<u8>> {
    let value = fs::read(path).map_err(|error| {
        AdapterError::unavailable(format!("cannot read {label} {}: {error}", path.display()))
    })?;
    if value.is_empty() || value.len() > limit {
        return Err(AdapterError::unavailable(format!(
            "{label} {} has an invalid length",
            path.display()
        )));
    }
    Ok(value)
}

fn current_boot_uuid() -> Result<String> {
    let path = Path::new("/proc/sys/kernel/random/boot_id");
    let value = bounded_file(path, BOOT_ID_BYTES, "boot identity")?;
    let value = std::str::from_utf8(&value)
        .map_err(|_| AdapterError::unavailable("Linux boot identity is not UTF-8"))?
        .trim();
    if !canonical_boot_uuid(value) {
        return Err(AdapterError::unavailable(
            "Linux boot identity is not a canonical UUID",
        ));
    }
    Ok(value.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LinuxProcessStat {
    state: u8,
    parent_pid: u64,
    process_group_id: u64,
    starttime_ticks: u64,
}

fn linux_process_state(state: u8) -> bool {
    matches!(
        state,
        b'R' | b'S' | b'D' | b'Z' | b'T' | b't' | b'X' | b'x' | b'K' | b'W' | b'P' | b'I'
    )
}

fn parse_process_stat(value: &[u8], expected_pid: u64) -> Result<LinuxProcessStat> {
    let bad =
        || AdapterError::unavailable(format!("cannot parse process stat for pid {expected_pid}"));
    let opening = value
        .windows(2)
        .position(|window| window == b" (")
        .ok_or_else(bad)?;
    let close = value
        .iter()
        .rposition(|byte| *byte == b')')
        .filter(|close| *close > opening + 1)
        .ok_or_else(bad)?;
    let state = match value.get(close + 1..close + 4) {
        Some([b' ', state, b' ']) if linux_process_state(*state) => *state,
        _ => return Err(bad()),
    };
    let recorded_pid = std::str::from_utf8(&value[..opening])
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value == expected_pid)
        .ok_or_else(|| {
            AdapterError::unavailable(format!(
                "process stat identity changed for pid {expected_pid}"
            ))
        })?;
    debug_assert_eq!(recorded_pid, expected_pid);
    let fields = value[close + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() <= 19 {
        return Err(bad());
    }
    let number = |field: &[u8]| {
        std::str::from_utf8(field)
            .ok()
            .and_then(|field| field.parse::<u64>().ok())
            .ok_or_else(bad)
    };
    Ok(LinuxProcessStat {
        state,
        parent_pid: number(fields[1])?,
        process_group_id: number(fields[2])?,
        starttime_ticks: number(fields[19])?,
    })
}

fn process_stat(pid: u64) -> Result<(u64, u64)> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let value = bounded_file(&path, PROC_STAT_BYTES, "process stat")?;
    let stat = parse_process_stat(&value, pid)?;
    if stat.state == b'Z' {
        return Err(AdapterError::unavailable(format!(
            "process stat for pid {pid} is incomplete or exited"
        )));
    }
    let process_group_id = Some(stat.process_group_id)
        .filter(|value| *value > 0 && *value <= i32::MAX as u64)
        .ok_or_else(|| {
            AdapterError::unavailable(format!("process group is invalid for pid {pid}"))
        })?;
    let starttime_ticks = Some(stat.starttime_ticks)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            AdapterError::unavailable(format!("process start time is invalid for pid {pid}"))
        })?;
    Ok((process_group_id, starttime_ticks))
}

#[cfg(target_os = "linux")]
fn open_pidfd_raw(pid: u64) -> io::Result<OwnedFd> {
    let pid = libc::pid_t::try_from(pid)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "custom process id is invalid")
        })?;
    for _attempt in 0..4 {
        // SAFETY: pidfd_open receives a checked positive pid and no pointer arguments.
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if descriptor >= 0 {
            // SAFETY: pidfd_open returned a new descriptor owned by this call.
            return Ok(unsafe { OwnedFd::from_raw_fd(descriptor as i32) });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::Interrupted,
        "pidfd_open interrupted repeatedly",
    ))
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: u64) -> Result<OwnedFd> {
    open_pidfd_raw(pid).map_err(|error| {
        AdapterError::unavailable(format!("cannot pin custom process {pid}: {error}"))
    })
}

#[cfg(target_os = "linux")]
fn pidfd_exited(pidfd: &OwnedFd) -> Result<bool> {
    for _attempt in 0..4 {
        let mut descriptor = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd for this call.
        let status = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if status == 0 && descriptor.revents == 0 {
            return Ok(false);
        }
        if status < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if status == 1
            && descriptor.revents & libc::POLLIN != 0
            && descriptor.revents & !(libc::POLLIN | libc::POLLHUP) == 0
        {
            return Ok(true);
        }
        return Err(AdapterError::unavailable(
            "cannot establish a valid pidfd exit indication",
        ));
    }
    Err(AdapterError::unavailable(
        "pidfd inspection interrupted repeatedly",
    ))
}

#[cfg(target_os = "linux")]
trait ProcessLivenessProbe {
    type Handle;

    fn boot_identity(&mut self) -> Result<String>;
    fn pin(&mut self, pid: u64) -> io::Result<Self::Handle>;
    fn exited(&mut self, handle: &Self::Handle) -> Result<bool>;
    fn process_stat(&mut self, pid: u64) -> Result<LinuxProcessStat>;
    fn executable_identity(&mut self, pid: u64) -> Result<(u64, u64)>;
}

#[cfg(target_os = "linux")]
struct KernelProcessLivenessProbe;

#[cfg(target_os = "linux")]
impl ProcessLivenessProbe for KernelProcessLivenessProbe {
    type Handle = OwnedFd;

    fn boot_identity(&mut self) -> Result<String> {
        current_boot_uuid()
    }

    fn pin(&mut self, pid: u64) -> io::Result<OwnedFd> {
        open_pidfd_raw(pid)
    }

    fn exited(&mut self, handle: &OwnedFd) -> Result<bool> {
        pidfd_exited(handle)
    }

    fn process_stat(&mut self, pid: u64) -> Result<LinuxProcessStat> {
        let path = PathBuf::from(format!("/proc/{pid}/stat"));
        let value = bounded_file(&path, PROC_STAT_BYTES, "process stat")?;
        let stat = parse_process_stat(&value, pid)?;
        if stat.starttime_ticks == 0
            || stat.parent_pid > i32::MAX as u64
            || stat.process_group_id > i32::MAX as u64
        {
            return Err(AdapterError::unavailable(
                "invalid Linux process generation",
            ));
        }
        Ok(stat)
    }

    fn executable_identity(&mut self, pid: u64) -> Result<(u64, u64)> {
        executable_identity(pid)
    }
}

#[cfg(target_os = "linux")]
fn process_liveness_with_probe<P: ProcessLivenessProbe>(
    expected: &CustomProcessIdentity,
    probe: &mut P,
) -> ProcessLiveness {
    fn inspect<P: ProcessLivenessProbe>(
        expected: &CustomProcessIdentity,
        probe: &mut P,
    ) -> Result<ProcessLiveness> {
        let boot = probe.boot_identity()?;
        if !canonical_boot_uuid(&boot) {
            return Ok(ProcessLiveness::Unknown);
        }
        let stable_boot = |probe: &mut P| {
            probe
                .boot_identity()
                .map(|after| canonical_boot_uuid(&after) && after == boot)
        };
        if boot != expected.boot_id {
            return Ok(if stable_boot(probe)? {
                ProcessLiveness::Dead
            } else {
                ProcessLiveness::Unknown
            });
        }
        let handle = match probe.pin(expected.pid) {
            Ok(handle) => handle,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
                return Ok(if stable_boot(probe)? {
                    ProcessLiveness::Dead
                } else {
                    ProcessLiveness::Unknown
                });
            }
            Err(_) => return Ok(ProcessLiveness::Unknown),
        };
        if probe.exited(&handle)? {
            return Ok(if stable_boot(probe)? {
                ProcessLiveness::Dead
            } else {
                ProcessLiveness::Unknown
            });
        }
        let first = probe.process_stat(expected.pid)?;
        if first.starttime_ticks == 0 || !linux_process_state(first.state) {
            return Ok(ProcessLiveness::Unknown);
        }
        let dead_state = |state| matches!(state, b'Z' | b'X' | b'x');
        if first.starttime_ticks != expected.starttime_ticks || dead_state(first.state) {
            let second = probe.process_stat(expected.pid)?;
            if second.starttime_ticks == 0 || !linux_process_state(second.state) {
                return Ok(ProcessLiveness::Unknown);
            }
            let stable_boot = stable_boot(probe)?;
            probe.exited(&handle)?;
            return Ok(
                if stable_boot
                    && first.starttime_ticks == second.starttime_ticks
                    && (first.starttime_ticks != expected.starttime_ticks
                        || dead_state(second.state))
                {
                    ProcessLiveness::Dead
                } else {
                    ProcessLiveness::Unknown
                },
            );
        }
        let image_before = probe.executable_identity(expected.pid)?;
        let second = probe.process_stat(expected.pid)?;
        if second.starttime_ticks == 0 || !linux_process_state(second.state) {
            return Ok(ProcessLiveness::Unknown);
        }
        let image_after = probe.executable_identity(expected.pid)?;
        let stable_boot = stable_boot(probe)?;
        let exited = probe.exited(&handle)?;
        if !stable_boot
            || first.starttime_ticks != second.starttime_ticks
            || image_before != image_after
        {
            return Ok(ProcessLiveness::Unknown);
        }
        if exited {
            return Ok(ProcessLiveness::Dead);
        }
        Ok(
            if !dead_state(second.state)
                && image_before == (expected.executable_device, expected.executable_inode)
            {
                ProcessLiveness::Alive
            } else {
                ProcessLiveness::Unknown
            },
        )
    }

    if !expected.valid() {
        return ProcessLiveness::Unknown;
    }
    inspect(expected, probe).unwrap_or(ProcessLiveness::Unknown)
}

#[cfg(target_os = "linux")]
fn require_live_pidfd(pidfd: &OwnedFd, pid: u64) -> Result<()> {
    let mut descriptor = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: descriptor points to one initialized pollfd for the duration of this call.
        let status = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if status == 0 && descriptor.revents == 0 {
            return Ok(());
        }
        if status < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if status < 0 {
            return Err(AdapterError::unavailable(format!(
                "cannot inspect pinned custom process {pid}: {}",
                io::Error::last_os_error()
            )));
        }
        return Err(AdapterError::unavailable(format!(
            "custom process {pid} exited while its identity was inspected"
        )));
    }
}

#[cfg(target_os = "linux")]
fn executable_identity(pid: u64) -> Result<(u64, u64)> {
    let path = PathBuf::from(format!("/proc/{pid}/exe"));
    let image = File::open(&path).map_err(|error| {
        AdapterError::unavailable(format!(
            "cannot open executable image for custom process {pid}: {error}"
        ))
    })?;
    let metadata = image.metadata().map_err(|error| {
        AdapterError::unavailable(format!(
            "cannot inspect executable image for custom process {pid}: {error}"
        ))
    })?;
    if !metadata.is_file() || metadata.dev() == 0 || metadata.ino() == 0 {
        return Err(AdapterError::unavailable(format!(
            "custom process {pid} has an invalid executable image"
        )));
    }
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(target_os = "linux")]
fn live_custom_process(pid: u64) -> Result<LiveCustomProcess> {
    if !(1..=i32::MAX as u64).contains(&pid) {
        return Err(AdapterError::unavailable(
            "custom process id is not a positive Linux process id",
        ));
    }
    let pidfd = open_pidfd(pid)?;
    require_live_pidfd(&pidfd, pid)?;
    let boot_before = current_boot_uuid()?;
    let stat_before = process_stat(pid)?;
    let executable_path = PathBuf::from(format!("/proc/{pid}/exe"));
    let executable_path_before = fs::read_link(&executable_path).map_err(|error| {
        AdapterError::unavailable(format!(
            "cannot read executable path for custom process {pid}: {error}"
        ))
    })?;
    let executable_before = executable_identity(pid)?;
    let stat_after = process_stat(pid)?;
    let executable_after = executable_identity(pid)?;
    let executable_path_after = fs::read_link(&executable_path).map_err(|error| {
        AdapterError::unavailable(format!(
            "cannot reread executable path for custom process {pid}: {error}"
        ))
    })?;
    let boot_after = current_boot_uuid()?;
    require_live_pidfd(&pidfd, pid)?;
    if boot_before != boot_after
        || stat_before != stat_after
        || executable_before != executable_after
        || executable_path_before != executable_path_after
    {
        return Err(AdapterError::unavailable(format!(
            "custom process {pid} identity changed while it was inspected"
        )));
    }
    Ok(LiveCustomProcess {
        identity: CustomProcessIdentity {
            version: 1,
            boot_id: boot_before,
            pid,
            starttime_ticks: stat_before.1,
            executable_device: executable_before.0,
            executable_inode: executable_before.1,
        },
        process_group_id: stat_before.0,
        executable_path: executable_path_before,
    })
}

#[cfg(not(target_os = "linux"))]
fn live_custom_process(_pid: u64) -> Result<LiveCustomProcess> {
    Err(AdapterError::unavailable(
        "custom process identity requires Linux pidfds and procfs",
    ))
}

fn supported_shell_name(path: &Path) -> bool {
    let Some(mut name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    if let Some(value) = name.strip_suffix(" (deleted)") {
        name = value;
    }
    SUPPORTED_PANE_SHELLS.contains(&name)
}

fn supported_shell_process(pid: u64) -> Result<Option<LiveCustomProcess>> {
    let observed = live_custom_process(pid)?;
    Ok(supported_shell_name(&observed.executable_path).then_some(observed))
}

fn process_task_ids(pid: u64) -> Result<Vec<String>> {
    let root = PathBuf::from(format!("/proc/{pid}/task"));
    let mut tasks = fs::read_dir(&root)
        .map_err(|error| {
            AdapterError::unavailable(format!(
                "cannot enumerate shell tasks for process {pid}: {error}"
            ))
        })?
        .map(|entry| {
            entry
                .map_err(|error| {
                    AdapterError::unavailable(format!(
                        "cannot inspect shell task for process {pid}: {error}"
                    ))
                })
                .and_then(|entry| {
                    entry.file_name().into_string().map_err(|_| {
                        AdapterError::unavailable(format!(
                            "shell task name for process {pid} is not UTF-8"
                        ))
                    })
                })
        })
        .collect::<Result<Vec<_>>>()?;
    tasks.sort();
    if tasks.is_empty()
        || tasks.len() > MAX_SHELL_TASKS
        || tasks.iter().any(|task| {
            task.is_empty()
                || !task.bytes().all(|byte| byte.is_ascii_digit())
                || task.parse::<u64>().ok().is_none_or(|value| value == 0)
        })
    {
        return Err(AdapterError::unavailable(format!(
            "shell task set for process {pid} is invalid"
        )));
    }
    Ok(tasks)
}

fn process_has_no_descendants(pid: u64) -> Result<bool> {
    let before = process_task_ids(pid)?;
    let mut children_supported = true;
    for task in &before {
        let path = PathBuf::from(format!("/proc/{pid}/task/{task}/children"));
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) =>
            {
                children_supported = false;
                break;
            }
            Err(error) => {
                return Err(AdapterError::unavailable(format!(
                    "cannot inspect shell descendants for process {pid}: {error}"
                )))
            }
        };
        let mut children = Vec::with_capacity(MAX_CHILDREN_BYTES.saturating_add(1));
        file.by_ref()
            .take((MAX_CHILDREN_BYTES + 1) as u64)
            .read_to_end(&mut children)
            .map_err(|error| {
                AdapterError::unavailable(format!(
                    "cannot read shell descendants for process {pid}: {error}"
                ))
            })?;
        if children.len() > MAX_CHILDREN_BYTES
            || !children.iter().all(|byte| byte.is_ascii_whitespace())
        {
            return Ok(false);
        }
    }
    if children_supported {
        return Ok(process_task_ids(pid)? == before);
    }

    fn proc_parents() -> Result<HashMap<u64, u64>> {
        let entries = fs::read_dir("/proc").map_err(|error| {
            AdapterError::unavailable(format!("cannot enumerate process table: {error}"))
        })?;
        let mut parents = HashMap::new();
        let mut numeric_entries = 0_usize;
        for entry in entries {
            let entry = entry.map_err(|error| {
                AdapterError::unavailable(format!("cannot inspect process table: {error}"))
            })?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            numeric_entries += 1;
            if numeric_entries > MAX_PROC_ENTRIES {
                return Err(AdapterError::unavailable(
                    "process table exceeds descendant-proof bound",
                ));
            }
            let process = name
                .parse::<u64>()
                .map_err(|_| AdapterError::unavailable("process table contains an invalid pid"))?;
            let raw = match fs::read(entry.path().join("stat")) {
                Ok(raw) => raw,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(libc::ESRCH) =>
                {
                    continue
                }
                Err(error) => {
                    return Err(AdapterError::unavailable(format!(
                        "cannot inspect process {process} parent: {error}"
                    )))
                }
            };
            if raw.len() > PROC_STAT_BYTES {
                return Err(AdapterError::unavailable(format!(
                    "process {process} stat is invalid"
                )));
            }
            let parent = parse_process_stat(&raw, process)?.parent_pid;
            if process > 0 {
                parents.insert(process, parent);
            }
        }
        Ok(parents)
    }

    fn has_descendant(pid: u64, parents: &HashMap<u64, u64>) -> bool {
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        for (&process, &parent) in parents {
            children.entry(parent).or_default().push(process);
        }
        let mut pending = children.get(&pid).cloned().unwrap_or_default();
        let mut seen = HashSet::new();
        while let Some(process) = pending.pop() {
            if seen.insert(process) {
                pending.extend(children.get(&process).into_iter().flatten().copied());
            }
        }
        !seen.is_empty()
    }

    let first = proc_parents()?;
    let second = proc_parents()?;
    Ok(!has_descendant(pid, &first)
        && !has_descendant(pid, &second)
        && process_task_ids(pid)? == before)
}

fn safe_executable_metadata(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
        && metadata.permissions().mode() & 0o111 != 0
        && metadata.permissions().mode() & 0o022 == 0
        && metadata.dev() > 0
        && metadata.ino() > 0
}

fn pin_harness_executable(path: PathBuf) -> Result<PinnedHarnessExecutable> {
    let mut image = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| {
            AdapterError::unavailable(format!(
                "cannot pin custom harness executable {}: {error}",
                path.display()
            ))
        })?;
    let metadata = image.metadata().map_err(|error| {
        AdapterError::unavailable(format!(
            "cannot inspect pinned custom harness executable {}: {error}",
            path.display()
        ))
    })?;
    if !safe_executable_metadata(&metadata) {
        return Err(AdapterError::unavailable(format!(
            "refusing unsafe pinned custom harness executable: {}",
            path.display()
        )));
    }
    let mut magic = [0_u8; 4];
    image.read_exact(&mut magic).map_err(|error| {
        AdapterError::unavailable(format!(
            "cannot read pinned custom harness executable {}: {error}",
            path.display()
        ))
    })?;
    if magic != *b"\x7fELF" {
        return Err(AdapterError::unavailable(format!(
            "custom harness executable must be an ELF image, not a script or binfmt payload: {}",
            path.display()
        )));
    }
    let revalidated = image.metadata().map_err(|error| {
        AdapterError::unavailable(format!(
            "cannot revalidate pinned custom harness executable {}: {error}",
            path.display()
        ))
    })?;
    let current = fs::metadata(&path).map_err(|error| {
        AdapterError::unavailable(format!(
            "cannot recheck custom harness executable {}: {error}",
            path.display()
        ))
    })?;
    if !safe_executable_metadata(&revalidated)
        || !safe_executable_metadata(&current)
        || revalidated.dev() != metadata.dev()
        || revalidated.ino() != metadata.ino()
        || current.dev() != metadata.dev()
        || current.ino() != metadata.ino()
    {
        return Err(AdapterError::unavailable(format!(
            "custom harness executable changed while it was pinned: {}",
            path.display()
        )));
    }
    Ok(PinnedHarnessExecutable {
        path,
        _image: image,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn muse_effort(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

/// Return only Muse's bounded downgrade sentence and its explicit effective effort.
pub(crate) fn muse_startup_metadata(screen: &str) -> (Option<String>, Option<String>) {
    for raw_line in screen.lines() {
        let line = raw_line.trim();
        if !line.is_ascii() || line.len() > 256 {
            continue;
        }
        let Some(rest) = line.strip_prefix("reasoning effort ") else {
            continue;
        };
        let Some((unavailable, effective)) = rest.rsplit_once("; using ") else {
            continue;
        };
        let Some((requested, detail)) = unavailable.split_once(" is not available") else {
            continue;
        };
        if !muse_effort(requested) || !muse_effort(effective) {
            continue;
        }
        // Either there is no trailing detail at all, or it is a parenthesised aside. Named rather
        // than written inline: clippy refuses the inline form as a non-minimal boolean, and its
        // own suggested rewrite folds the two acceptable shapes into one negated disjunction that
        // no longer says which shapes are acceptable.
        let detail_is_acceptable =
            detail.is_empty() || (detail.starts_with(" (") && detail.ends_with(')'));
        if !detail_is_acceptable {
            continue;
        }
        return (Some(line.to_owned()), Some(effective.to_owned()));
    }
    (None, None)
}

/// Recognize only explicit workspace-trust questions.
pub(crate) fn muse_trust_prompt(screen: &str) -> bool {
    let lowered = screen.to_ascii_lowercase();
    lowered.contains("trust this workspace")
        || (lowered.contains("do you trust")
            && (lowered.contains("workspace") || lowered.contains("folder")))
        || lowered.contains("workspace trust")
}

/// Muse's ruled editor, its status order and the proofs that input reached it.
mod muse_editor;
pub(crate) use muse_editor::*;

#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Capture a subprocess while enforcing wall-clock and byte bounds on multiplexed output pipes.
pub(crate) fn bounded_output(command: Command, timeout: Duration) -> io::Result<BoundedOutput> {
    bounded_output_with_cancellation(command, timeout, &|| false)
}

fn bounded_output_with_cancellation(
    command: Command,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<BoundedOutput> {
    bounded_output_with_cancellation_and_shutdown(
        command,
        timeout,
        cancelled,
        &mut ProcessPluginChild::shutdown,
    )
}

fn bounded_output_with_cancellation_and_shutdown(
    command: Command,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
    shutdown: &mut dyn FnMut(&mut ProcessPluginChild, Duration) -> io::Result<ExitStatus>,
) -> io::Result<BoundedOutput> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "control command timeout is too large",
        )
    })?;
    let mut captured = CapturedProcess::spawn(command)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    loop {
        let was_cancelled = cancelled();
        if was_cancelled || Instant::now() >= deadline {
            // ProcessPluginChild's live atomic supervisor pins the private process-group identity
            // until shutdown has signalled the group. In particular, no reaped command leader can
            // turn this cleanup into a signal to a numerically reused process group. Escaped
            // setsid descendants can retain their copies of these pipes, but dropping our readers
            // below is nonblocking and does not wait for their EOF.
            let primary = if was_cancelled {
                io::Error::new(io::ErrorKind::Interrupted, "control command was cancelled")
            } else {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("control command timed out after {}s", timeout.as_secs()),
                )
            };
            return match shutdown(&mut captured.child, Duration::ZERO) {
                Ok(_) => Err(primary),
                Err(cleanup) => Err(io::Error::new(
                    primary.kind(),
                    format!("{primary}; process cleanup is uncertain: {cleanup}"),
                )),
            };
        }
        if !stdout_eof {
            stdout_eof = drain_available(
                &mut captured.stdout,
                &mut stdout,
                CONTROL_STDOUT_BYTES,
                "stdout",
            )?;
        }
        if !stderr_eof {
            stderr_eof = drain_available(
                &mut captured.stderr,
                &mut stderr,
                CONTROL_STDERR_BYTES,
                "stderr",
            )?;
        }
        // executable_status uses waitid(P_PIDFD, WNOWAIT): observing exit does not release the
        // executable's numeric identity before the pinned private group has been terminated.
        if stdout_eof && stderr_eof && captured.child.executable_status()?.is_some() {
            let status = shutdown(&mut captured.child, Duration::ZERO)?;
            return Ok(BoundedOutput {
                status,
                stdout,
                stderr,
            });
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        poll_capture(
            (!stdout_eof).then_some(&captured.stdout),
            (!stderr_eof).then_some(&captured.stderr),
            remaining.min(CAPTURE_POLL_INTERVAL),
        )?;
    }
}

struct CapturedProcess {
    child: ProcessPluginChild,
    stdout: File,
    stderr: File,
}

impl CapturedProcess {
    #[cfg(target_os = "linux")]
    fn spawn(mut command: Command) -> io::Result<Self> {
        let (stderr, stderr_writer) = pipe_cloexec()?;
        // ProcessPluginChild deliberately sends a protocol plugin's stderr to /dev/null. Herdr is
        // an ordinary control command, so restore this private capture pipe after Command has
        // installed its standard descriptors but before exec. The captured OwnedFd keeps the
        // writer live in the pre-exec child and O_CLOEXEC closes the extra copy on successful exec.
        unsafe {
            command.pre_exec(move || loop {
                if libc::dup2(stderr_writer.as_raw_fd(), libc::STDERR_FILENO) >= 0 {
                    return Ok(());
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            });
        }
        let mut child = ProcessPluginChild::spawn(command)?;
        let (stdout, stdin) = child.take_transport()?;
        drop(stdin);
        set_nonblocking(&stdout)?;
        set_nonblocking(&stderr)?;
        Ok(Self {
            child,
            stdout,
            stderr,
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn spawn(_command: Command) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "bounded control commands require Linux pidfd supervision",
        ))
    }
}

#[cfg(target_os = "linux")]
fn pipe_cloexec() -> io::Result<(File, OwnedFd)> {
    let mut descriptors = [-1_i32; 2];
    // SAFETY: descriptors points to writable storage for exactly two file descriptors.
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pipe2 returned two fresh descriptors owned by this function.
    Ok(unsafe {
        (
            File::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

fn set_nonblocking(file: &File) -> io::Result<()> {
    // SAFETY: fcntl receives a live descriptor and F_GETFL has no third argument.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFL accepts the retrieved flags plus O_NONBLOCK for the same descriptor.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_available(
    reader: &mut File,
    output: &mut Vec<u8>,
    limit: usize,
    stream: &str,
) -> io::Result<bool> {
    let mut drained = 0;
    let mut buffer = [0_u8; 16 * 1024];
    while drained < CAPTURE_READ_BURST {
        let remaining = limit.saturating_sub(output.len());
        let read_bound = remaining.saturating_add(1).min(buffer.len());
        match reader.read(&mut buffer[..read_bound]) {
            Ok(0) => return Ok(true),
            Ok(count) if count > remaining => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("control command {stream} exceeds its {limit}-byte limit"),
                ));
            }
            Ok(count) => {
                output.extend_from_slice(&buffer[..count]);
                drained += count;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn poll_capture(stdout: Option<&File>, stderr: Option<&File>, timeout: Duration) -> io::Result<()> {
    let mut descriptors = [
        libc::pollfd {
            fd: stdout.map_or(-1, AsRawFd::as_raw_fd),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stderr.map_or(-1, AsRawFd::as_raw_fd),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let milliseconds = timeout
        .as_millis()
        .max(u128::from(!timeout.is_zero()))
        .min(i32::MAX as u128) as i32;
    // SAFETY: descriptors is initialized and remains live for the matching array length.
    let result = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            descriptors.len() as libc::nfds_t,
            milliseconds,
        )
    };
    if result >= 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // Return EINTR to the absolute-deadline owner loop instead of restarting the full relative
    // timeout here; a signal stream must not extend control-command cancellation indefinitely.
    if error.kind() == io::ErrorKind::Interrupted {
        #[cfg(all(test, target_os = "linux"))]
        POLL_CAPTURE_INTERRUPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        return Ok(());
    }
    Err(error)
}

/// One terminal pane and its owning tab and workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pane {
    /// Herdr pane identifier.
    pub pane_id: String,
    /// Herdr tab identifier.
    pub tab_id: String,
    /// Herdr workspace identifier.
    pub workspace_id: String,
}

/// Identity and readiness fields for one interactive-agent pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentPaneInfo {
    /// Herdr pane identifier.
    pub pane_id: String,
    /// Owning workspace identifier.
    pub workspace_id: String,
    /// Pane working directory as reported by Herdr.
    pub cwd: String,
    /// Interactive agent implementation, when Herdr recognizes one.
    pub agent: Option<String>,
    /// Native Herdr agent status, or `"unknown"` when no status was reported.
    pub status: String,
    /// Agent name carried by the stable session identity, when present.
    pub session_agent: Option<String>,
    /// Stable session value, when present.
    pub session_value: Option<String>,
    /// Scrollback geometry, when Herdr reports it.
    pub scroll: Option<PaneScroll>,
    /// Herdr terminal the pane holds, when `pane get` reported it.
    pub terminal_id: Option<String>,
    /// Herdr tab that contains the pane, when `pane get` reported it.
    pub tab_id: Option<String>,
}

/// What `agent get NAME` reports about one named Herdr agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentIdentity {
    /// Exact Herdr agent name.
    pub name: String,
    /// Pane the named agent occupies.
    pub pane_id: String,
    /// Tab of that pane, when reported.
    pub tab_id: Option<String>,
    /// Terminal of that pane, when reported.
    pub terminal_id: Option<String>,
}

impl AgentPaneInfo {
    /// Whether Herdr reports that the pane keeps no scrollback, so its visible screen is all it
    /// holds. A full-screen program such as Claude Code redraws one screen in place and keeps
    /// none. An unreported geometry is unknown, not none.
    pub fn keeps_no_scrollback(&self) -> bool {
        self.scroll
            .is_some_and(|scroll| scroll.max_offset_from_bottom == 0)
    }
}

/// Scrollback geometry of one pane, in terminal rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaneScroll {
    /// Rows the view is scrolled up from the bottom.
    pub offset_from_bottom: u64,
    /// Most rows the view can scroll up: the scrollback the pane keeps.
    pub max_offset_from_bottom: u64,
    /// Rows on the visible screen.
    pub viewport_rows: u64,
}

/// Captured result of one external command invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    /// Conventional numeric process status; zero denotes success.
    pub status: i32,
    /// Captured standard output decoded as UTF-8 with replacement.
    pub stdout: String,
    /// Captured standard error decoded as UTF-8 with replacement.
    pub stderr: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentStartupTimeout {
    id: String,
    error: AgentStartupTimeoutError,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentStartupTimeoutError {
    code: String,
    message: String,
}

fn codex_startup_timeout(output: &CommandOutput) -> bool {
    output.status == 1
        && output.stdout.trim().is_empty()
        && serde_json::from_str::<AgentStartupTimeout>(&output.stderr).is_ok_and(|response| {
            response.id == "cli:agent:start"
                && response.error.code == "timeout"
                && response.error.message == "timed out waiting for agent startup"
        })
}

/// Direct Herdr CLI adapter; executable availability is checked when used.
#[derive(Clone, Debug)]
pub struct HerdrClient {
    executable: PathBuf,
    /// Whether the running server advertises `input-expect`, read once.
    input_expect: std::sync::OnceLock<bool>,
}
impl HerdrClient {
    /// Select a configured Herdr executable. Only the direct adapter is supported.
    pub fn with_executable(adapter: &str, executable: &Path) -> Result<Self> {
        if adapter != "direct" {
            return Err(AdapterError::unavailable(
                "agentctl supports direct Herdr access only",
            ));
        }
        Ok(Self {
            executable: executable.to_owned(),
            input_expect: std::sync::OnceLock::new(),
        })
    }
    fn invoke_with_timeout(&self, args: &[String], timeout: Duration) -> Result<CommandOutput> {
        self.invoke_with_timeout_and_cancellation(args, timeout, &|| false)
    }
    fn invoke_with_timeout_and_cancellation(
        &self,
        args: &[String],
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<CommandOutput> {
        let executable = resolve_executable(&self.executable)?;
        let mut command = Command::new(executable);
        command.args(args);
        let output = bounded_output_with_cancellation(command, timeout, cancelled)
            .map_err(|error| AdapterError::unavailable(format!("cannot invoke Herdr: {error}")))?;
        Ok(CommandOutput {
            status: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn workspace_id_for_label(&self, label: &str) -> Result<Option<String>> {
        let workspaces = self.workspace_entries()?;
        unique_label_id(&workspaces, "workspace_id", label, "workspace")
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn create_workspace(
        &self,
        label: &str,
        cwd: &str,
        environment: &[String],
    ) -> Result<(String, String, String)> {
        let arguments = workspace_create_arguments(label, cwd, environment);
        let result = self.call(&arguments, &format!("workspace create {label:?}"))?;
        let workspace = required_object(&result, "workspace", "workspace create")?;
        let tab = required_object(&result, "tab", "workspace create")?;
        let pane = required_object(&result, "root_pane", "workspace create")?;
        Ok((
            required_string(workspace, "workspace_id", "workspace create")?,
            required_string(tab, "tab_id", "workspace create")?,
            required_string(pane, "pane_id", "workspace create")?,
        ))
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn create_tab(
        &self,
        workspace_id: &str,
        label: &str,
        cwd: &str,
        environment: &[String],
    ) -> Result<String> {
        let arguments = tab_create_arguments(workspace_id, label, cwd, environment);
        let result = self.call(&arguments, &format!("tab create {label:?}"))?;
        let tab = match result.get("tab") {
            Some(value) => value
                .as_object()
                .ok_or_else(|| AdapterError::unavailable("tab create: 'tab' is not an object"))?,
            None => &result,
        };
        required_string(tab, "tab_id", "tab create")
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn rename_tab(&self, tab_id: &str, label: &str) -> Result<()> {
        self.call(
            &strings(&["tab", "rename", tab_id, label]),
            &format!("tab rename {tab_id}"),
        )?;
        Ok(())
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn close_tab(&self, tab_id: &str) -> Result<()> {
        self.call_ok(
            &strings(&["tab", "close", tab_id]),
            &format!("tab close {tab_id}"),
        )
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn start_agent(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        arguments: &[String],
        timeout: Duration,
    ) -> Result<()> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(AdapterError::unavailable(
                "agent startup timeout must be between 0 and 300 seconds",
            ));
        }
        // This observation is required only for the additional readiness proof. A native
        // successful launch does not depend on the fallback's inspection being available.
        let initial = if kind == "codex" {
            self.pane_info(pane_id).ok()
        } else {
            None
        };
        let mut args = strings(&[
            "agent",
            "start",
            name,
            "--kind",
            kind,
            "--pane",
            pane_id,
            "--timeout",
        ]);
        args.extend([timeout.as_millis().max(1).to_string(), "--".to_owned()]);
        args.extend_from_slice(arguments);
        let result = self.invoke_with_timeout(&args, timeout + CONTROL_TIMEOUT)?;
        if result.status != 0 {
            if codex_startup_timeout(&result)
                && initial.as_ref().is_some_and(|initial| {
                    self.codex_ready_after_startup_timeout(name, initial)
                        .unwrap_or(false)
                })
            {
                return Ok(());
            }
            return Err(AdapterError::unavailable(format!(
                "agent start {name:?}: {}",
                stderr_detail(&result)
            )));
        }
        Ok(())
    }

    fn codex_ready_after_startup_timeout(
        &self,
        name: &str,
        initial: &AgentPaneInfo,
    ) -> Result<bool> {
        if initial.agent.is_some()
            || initial
                .terminal_id
                .as_deref()
                .is_none_or(|value| value.is_empty() || value.contains('\0'))
        {
            return Ok(false);
        }
        let info = self.pane_info(&initial.pane_id)?;
        if info.pane_id != initial.pane_id
            || info.workspace_id != initial.workspace_id
            || info.cwd != initial.cwd
            || info.tab_id != initial.tab_id
            || info.terminal_id != initial.terminal_id
        {
            return Ok(false);
        }
        let named = self.agent_identity(name)?;
        if named.pane_id != info.pane_id
            || named.tab_id != info.tab_id
            || named.terminal_id != info.terminal_id
        {
            return Ok(false);
        }
        let Some(identity) = self.harness_identity(&info.pane_id, "codex")? else {
            return Ok(false);
        };
        if !self.codex_idle_ready_with_cancellation(&info, &|| false)? {
            return Ok(false);
        }
        Ok(self.agent_identity(name)? == named
            && self.pane_info(&info.pane_id)? == info
            && self.verify_harness_identity(&info.pane_id, &identity)?)
    }

    fn pane_process_state(
        &self,
        pane_id: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PaneProcessState> {
        let result = self.call_with_cancellation(
            &strings(&["pane", "process-info", "--pane", pane_id]),
            &format!("pane process-info {pane_id}"),
            cancelled,
        )?;
        let info = required_object(&result, "process_info", "pane process-info")?;
        if required_string(info, "pane_id", "pane process-info")? != pane_id {
            return Err(AdapterError::unavailable(
                "pane process-info returned a different pane identity",
            ));
        }
        let shell_pid = info
            .get("shell_pid")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0 && *value <= i32::MAX as u64)
            .ok_or_else(|| {
                AdapterError::unavailable(
                    "pane process-info: shell_pid is not a positive Linux process id",
                )
            })?;
        let foreground_process_group_id = info
            .get("foreground_process_group_id")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0 && *value <= i32::MAX as u64)
            .ok_or_else(|| {
                AdapterError::unavailable(
                    "pane process-info: foreground_process_group_id is not a positive Linux process id",
                )
            })?;
        let processes = value_array(info.get("foreground_processes"), "foreground_processes")?;
        Ok(PaneProcessState {
            shell_pid,
            foreground_process_group_id,
            processes: foreground_process_entries(processes)?,
        })
    }

    fn foreground_processes(
        &self,
        pane_id: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(u64, Vec<Map<String, Value>>)> {
        let result = self.call_with_cancellation(
            &strings(&["pane", "process-info", "--pane", pane_id]),
            &format!("pane process-info {pane_id}"),
            cancelled,
        )?;
        let info = required_object(&result, "process_info", "pane process-info")?;
        if required_string(info, "pane_id", "pane process-info")? != pane_id {
            return Err(AdapterError::unavailable(
                "pane process-info returned a different pane identity",
            ));
        }
        let foreground_process_group_id = info
            .get("foreground_process_group_id")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0 && *value <= i32::MAX as u64)
            .ok_or_else(|| {
                AdapterError::unavailable(
                    "pane process-info: foreground_process_group_id is not a positive Linux process id",
                )
            })?;
        let processes = value_array(info.get("foreground_processes"), "foreground_processes")?;
        Ok((
            foreground_process_group_id,
            foreground_process_entries(processes)?,
        ))
    }

    fn custom_harness_observed(
        &self,
        pane_id: &str,
        executable: &PinnedHarnessExecutable,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<CustomProcessIdentity>> {
        let (foreground_process_group_id, processes) =
            self.foreground_processes(pane_id, cancelled)?;
        let mut candidate = None;
        for process in processes {
            let pid = process
                .get("pid")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0 && *value <= i32::MAX as u64)
                .ok_or_else(|| {
                    AdapterError::unavailable(
                        "foreground process: \"pid\" is not a positive Linux process id",
                    )
                })?;
            if foreground_argv0(&process)?.is_some_and(|argv0| {
                !fs::canonicalize(argv0).is_ok_and(|path| path == executable.path.as_path())
            }) {
                continue;
            }
            let observed = live_custom_process(pid)?;
            if observed.process_group_id != foreground_process_group_id
                || observed.identity.executable_device != executable.device
                || observed.identity.executable_inode != executable.inode
            {
                continue;
            }
            // Omitted argv carries no executable authority. The already-open launch image
            // and a coherent kernel observation can still establish this exact process.
            if candidate.replace(observed.identity).is_some() {
                return Err(AdapterError::unavailable(
                    "pane process-info returned multiple matching custom harness processes",
                ));
            }
        }
        Ok(candidate)
    }

    fn recorded_custom_harness_observed(
        &self,
        pane_id: &str,
        identity: &CustomProcessIdentity,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<bool> {
        if !identity.valid() {
            return Err(AdapterError::unavailable(
                "recorded custom process identity is invalid",
            ));
        }
        let (foreground_process_group_id, processes) =
            self.foreground_processes(pane_id, cancelled)?;
        let mut matches = 0_u8;
        for process in processes {
            let pid = process
                .get("pid")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0 && *value <= i32::MAX as u64)
                .ok_or_else(|| {
                    AdapterError::unavailable(
                        "foreground process: \"pid\" is not a positive Linux process id",
                    )
                })?;
            if pid == identity.pid {
                matches = matches.saturating_add(1);
            }
        }
        if matches != 1 {
            return Ok(false);
        }
        let observed = live_custom_process(identity.pid)?;
        Ok(observed.process_group_id == foreground_process_group_id
            && observed.identity == *identity)
    }

    /// Require the fixed-location custom harness executable in one exact foreground pane.
    pub fn verify_custom_harness(
        &self,
        pane_id: &str,
        kind: &str,
        identity: Option<&CustomProcessIdentity>,
    ) -> Result<()> {
        self.verify_custom_harness_with_cancellation(pane_id, kind, identity, &|| false)
    }

    /// Prove that no child command owns the terminal and the foreground process
    /// group contains only the pane's Herdr-reported shell process.
    pub fn pane_is_idle_shell(&self, pane_id: &str) -> Result<bool> {
        Ok(self.pane_idle_shell_identity(pane_id)?.is_some())
    }

    fn idle_shell_proof(&self, state: &PaneProcessState) -> Result<Option<PaneShellProof>> {
        if state.foreground_process_group_id != state.shell_pid || state.processes.len() != 1 {
            return Ok(None);
        }
        let process = &state.processes[0];
        let pid = process
            .get("pid")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0 && *value <= i32::MAX as u64)
            .ok_or_else(|| {
                AdapterError::unavailable(
                    "foreground process: \"pid\" is not a positive Linux process id",
                )
            })?;
        let argv0 = process
            .get("argv")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
            .and_then(Value::as_str)
            .unwrap_or_default();
        let argv_executable = fs::canonicalize(argv0).ok();
        let kernel_executable = fs::canonicalize(format!("/proc/{}/exe", state.shell_pid)).ok();
        let Some(observed) = supported_shell_process(state.shell_pid)? else {
            return Ok(None);
        };
        if pid != state.shell_pid
            || argv_executable.is_none()
            || argv_executable != kernel_executable
            || observed.process_group_id != state.shell_pid
            || !process_has_no_descendants(state.shell_pid)?
        {
            return Ok(None);
        }
        let Some(confirmed) = supported_shell_process(state.shell_pid)? else {
            return Ok(None);
        };
        if confirmed.identity != observed.identity
            || confirmed.process_group_id != observed.process_group_id
            || confirmed.executable_path != observed.executable_path
        {
            return Ok(None);
        }
        Ok(Some(PaneShellProof {
            identity: observed.identity,
            executable_path: observed.executable_path,
        }))
    }

    /// Capture one stable supported idle-shell generation with no descendants.
    pub fn pane_idle_shell_identity(&self, pane_id: &str) -> Result<Option<PaneShellProof>> {
        let before = self.pane_process_state(pane_id, &|| false)?;
        let Some(proof) = self.idle_shell_proof(&before)? else {
            return Ok(None);
        };
        let after = self.pane_process_state(pane_id, &|| false)?;
        if after != before || self.idle_shell_proof(&after)?.as_ref() != Some(&proof) {
            return Ok(None);
        }
        Ok(Some(proof))
    }

    /// Capture the exact Linux process generation and executable image for a pane shell.
    pub fn pane_shell_identity(&self, pane_id: &str) -> Result<CustomProcessIdentity> {
        let state = self.pane_process_state(pane_id, &|| false)?;
        let observed = supported_shell_process(state.shell_pid)?
            .filter(|value| value.process_group_id == state.shell_pid)
            .ok_or_else(|| {
                AdapterError::unavailable(format!(
                    "cannot capture a supported identity-bound shell process for pane {pane_id}"
                ))
            })?;
        Ok(observed.identity)
    }

    /// Require the recorded shell generation and Herdr's strict idle-shell presentation.
    pub fn pane_is_same_idle_shell(
        &self,
        pane_id: &str,
        expected: &CustomProcessIdentity,
    ) -> Result<bool> {
        if !expected.valid() {
            return Err(AdapterError::unavailable(
                "recorded pane shell identity is invalid",
            ));
        }
        Ok(self
            .pane_idle_shell_identity(pane_id)?
            .is_some_and(|proof| proof.identity == *expected))
    }

    pub(crate) fn verify_custom_harness_with_cancellation(
        &self,
        pane_id: &str,
        kind: &str,
        identity: Option<&CustomProcessIdentity>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        let observed = if let Some(identity) = identity {
            // Do not consult the installation pathname here. An upgrade may have atomically
            // replaced it while this still-running process retains the recorded image inode.
            self.recorded_custom_harness_observed(pane_id, identity, cancelled)?
        } else {
            // Records without a durable process tuple retain strict current-path
            // check; never interpret a missing identity as a wildcard for a deleted executable.
            let executable = pin_harness_executable(resolve_harness_executable(kind)?)?;
            self.custom_harness_observed(pane_id, &executable, cancelled)?
                .is_some()
        };
        if !observed {
            return Err(AdapterError::unavailable(format!(
                "custom harness {kind:?} is not the foreground process in pane {pane_id}"
            )));
        }
        Ok(())
    }

    /// Launch an otherwise unsupported TUI through one exact pane.
    ///
    /// This path never supplies input to a workspace-trust prompt. It proves
    /// the foreground executable, publishes a pane-local agent report, and
    /// re-reads that exact pane rather than consulting a name registry.
    pub fn start_pane_agent(
        &self,
        _name: &str,
        kind: &str,
        pane_id: &str,
        arguments: &[String],
        timeout: Duration,
        persist_identity: &mut dyn FnMut(CustomProcessIdentity) -> Result<()>,
    ) -> Result<()> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(AdapterError::unavailable(
                "agent startup timeout must be between 0 and 300 seconds",
            ));
        }
        if kind.is_empty()
            || !kind.as_bytes()[0].is_ascii_lowercase()
            || !kind
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(AdapterError::unavailable(
                "custom harness name must contain lowercase ASCII letters, digits, or hyphens",
            ));
        }
        let executable = pin_harness_executable(resolve_harness_executable(kind)?)?;
        let mut command = vec![executable.path.display().to_string()];
        command.extend_from_slice(arguments);
        let line = shell_join(&command)?;
        self.call_ok(
            &[
                "pane".to_owned(),
                "run".to_owned(),
                pane_id.to_owned(),
                line,
            ],
            &format!("pane run {kind:?}"),
        )?;
        let deadline = Instant::now() + timeout;
        let mut observed = None;
        while Instant::now() < deadline {
            observed = self.custom_harness_observed(pane_id, &executable, &|| false)?;
            if observed.is_some() {
                break;
            }
            thread::sleep(
                Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        let observed = observed.ok_or_else(|| {
            AdapterError::unavailable(format!(
                "custom harness {kind:?} was not the foreground process in pane {pane_id}"
            ))
        })?;
        // Persist before trust/readiness/report checks. If any later step fails, stop still has
        // enough kernel identity to close only this observed process's pane.
        persist_identity(observed.clone())?;
        let mut ready = false;
        while Instant::now() < deadline {
            if !self.recorded_custom_harness_observed(pane_id, &observed, &|| false)? {
                return Err(AdapterError::unavailable(format!(
                    "custom harness {kind:?} identity changed before readiness in pane {pane_id}"
                )));
            }
            let screen = self.read(pane_id, "visible", Some(200))?;
            if muse_trust_prompt(&screen)
                && !arguments.iter().any(|value| value == "--trust-workspace")
            {
                return Err(AdapterError::unavailable(format!(
                    "{kind} workspace trust prompt requires human attention; no input was submitted"
                )));
            }
            ready = muse_idle_composer(&screen);
            if ready {
                break;
            }
            thread::sleep(
                Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        if !ready {
            return Err(AdapterError::unavailable(format!(
                "custom harness {kind:?} did not reach a verified idle composer in pane {pane_id}"
            )));
        }
        if !self.recorded_custom_harness_observed(pane_id, &observed, &|| false)? {
            return Err(AdapterError::unavailable(format!(
                "custom harness {kind:?} identity changed before report in pane {pane_id}"
            )));
        }
        self.call_ok(
            &strings(&[
                "pane",
                "report-agent",
                pane_id,
                "--source",
                "agentctl",
                "--agent",
                kind,
                "--state",
                "idle",
                "--message",
                "agentctl custom harness",
            ]),
            &format!("report custom agent {pane_id}"),
        )?;
        Ok(())
    }
    /// Run one literal executable through `pane run` and pin its foreground process identity.
    ///
    /// The executable must be a same-user-or-root, non-writable ELF image, exactly as for custom
    /// harnesses. No agent state is reported to Herdr: the command is a viewer, and a synthetic
    /// report would go stale the moment the viewed session changed state.
    pub fn start_pane_command(
        &self,
        pane_id: &str,
        executable: &Path,
        arguments: &[String],
        timeout: Duration,
    ) -> Result<CustomProcessIdentity> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(AdapterError::unavailable(
                "pane command startup timeout must be between 0 and 300 seconds",
            ));
        }
        let canonical = fs::canonicalize(executable).map_err(|error| {
            AdapterError::unavailable(format!(
                "cannot resolve pane command {}: {error}",
                executable.display()
            ))
        })?;
        let pinned = pin_harness_executable(canonical)?;
        let mut command = vec![pinned.path.display().to_string()];
        command.extend_from_slice(arguments);
        let line = shell_join(&command)?;
        self.call_ok(
            &[
                "pane".to_owned(),
                "run".to_owned(),
                pane_id.to_owned(),
                line,
            ],
            &format!("pane run {}", pinned.path.display()),
        )?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(identity) = self.custom_harness_observed(pane_id, &pinned, &|| false)? {
                return Ok(identity);
            }
            if Instant::now() >= deadline {
                return Err(AdapterError::unavailable(format!(
                    "{} was not the foreground process in pane {pane_id} after {}s; read the pane for its error",
                    pinned.path.display(),
                    timeout.as_secs_f64()
                )));
            }
            thread::sleep(
                Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    /// Replace a fresh pane's shell with a slot-boxed shell and return its PID.
    ///
    /// `command_line` comes from `wrkslots shell-command` and execs at every step, so the
    /// boxed interactive shell keeps the pane's shell PID. Readiness is proven from kernel
    /// state, not screen text: the pane's own shell PID is the sole foreground process, it
    /// is a shell again, and its cgroup is a wrkslots slice.
    pub fn enter_slot_sandbox(
        &self,
        pane_id: &str,
        command_line: &str,
        timeout: Duration,
    ) -> Result<u64> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(AdapterError::unavailable(
                "slot sandbox timeout must be between 0 and 300 seconds",
            ));
        }
        self.call_ok(
            &[
                "pane".to_owned(),
                "run".to_owned(),
                pane_id.to_owned(),
                command_line.to_owned(),
            ],
            &format!("pane run wrkslots sandbox {pane_id}"),
        )?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            // A process mid-exec can briefly report no argv; retry.
            if let Ok(state) = self.pane_process_state(pane_id, &|| false) {
                if slot_boxed_shell(&state) {
                    thread::sleep(Duration::from_millis(300)); // let the new shell draw its prompt
                    return Ok(state.shell_pid);
                }
            }
            thread::sleep(
                Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        Err(AdapterError::unavailable(format!(
            "pane {pane_id} did not return to a wrkslots-boxed shell within {}s; inspect the pane for the wrkslots error",
            timeout.as_secs_f64()
        )))
    }

    /// Identify the program a slot's root relay runs in one pane, if any.
    ///
    /// Under root slot isolation the pane's shell PID execs into sudo, which keeps the
    /// terminal and runs the boxed program on a private pty behind a monitor process. The
    /// program is the first process below the pane's shell PID that belongs to this user and
    /// shares its wrkslots slot scope. The pane lives exactly as long as that chain.
    pub fn relay_process(&self, pane_id: &str) -> Result<Option<CustomProcessIdentity>> {
        let state = self.pane_process_state(pane_id, &|| false)?;
        let argv0 = state
            .processes
            .first()
            .and_then(|process| process.get("argv"))
            .and_then(Value::as_array)
            .and_then(|values| values.first())
            .and_then(Value::as_str)
            .unwrap_or_default();
        if state.processes.len() != 1
            || state.processes[0].get("pid").and_then(Value::as_u64) != Some(state.shell_pid)
            || state.foreground_process_group_id != state.shell_pid
            || argv0.rsplit('/').next() != Some("sudo")
        {
            return Ok(None);
        }
        let cgroup = |pid: u64| fs::read_to_string(format!("/proc/{pid}/cgroup")).ok();
        let Some(scope) = cgroup(state.shell_pid).filter(|scope| scope.contains("wrkslots")) else {
            return Ok(None);
        };
        let uid = fs::metadata("/proc/self")
            .map(|metadata| metadata.uid())
            .map_err(|error| {
                AdapterError::unavailable(format!("cannot read this process's owner: {error}"))
            })?;
        let tree = process_children_map();
        let mut frontier = vec![state.shell_pid];
        let mut seen = HashSet::from([state.shell_pid]);
        for _depth in 0..4 {
            let mut following = Vec::new();
            for parent in frontier {
                let mut children = tree.get(&parent).cloned().unwrap_or_default();
                children.sort_unstable();
                for child in children {
                    if !seen.insert(child) {
                        continue;
                    }
                    let owner =
                        fs::metadata(format!("/proc/{child}")).map(|metadata| metadata.uid());
                    if owner.is_ok_and(|owner| owner == uid)
                        && cgroup(child).as_ref() == Some(&scope)
                    {
                        return Ok(live_custom_process(child)
                            .ok()
                            .map(|process| process.identity));
                    }
                    following.push(child);
                }
            }
            frontier = following;
        }
        Ok(None)
    }

    /// Require the recorded program to still run behind this pane's root relay.
    pub fn verify_relay_harness(
        &self,
        pane_id: &str,
        expected: Option<&CustomProcessIdentity>,
    ) -> Result<()> {
        let observed = self.relay_process(pane_id)?;
        if observed.is_none() || observed.as_ref() != expected {
            return Err(AdapterError::unavailable(format!(
                "the recorded program is not running behind the slot relay in pane {pane_id}"
            )));
        }
        Ok(())
    }

    /// Herdr's live screen-rule verdict for one pane: (agent kind, state).
    ///
    /// Herdr keeps a reported pane state as reported; its bundled screen rules are evaluated
    /// live only by `agent explain`.
    pub fn explain_agent(&self, pane_id: &str) -> Result<(Option<String>, String)> {
        let completed =
            self.invoke_with_timeout(&strings(&["agent", "explain", pane_id]), CONTROL_TIMEOUT)?;
        if completed.status != 0 {
            return Err(AdapterError::unavailable(format!(
                "agent explain {pane_id}: {}",
                detail(&completed)
            )));
        }
        let mut agent = None;
        let mut state = None;
        let mut rule = None;
        let mut fallback = false;
        for line in completed.stdout.lines() {
            if let Some((key, value)) = line.split_once(':') {
                match key.trim() {
                    "agent" if agent.is_none() => agent = Some(value.trim().to_owned()),
                    "state" if state.is_none() => state = Some(value.trim().to_owned()),
                    "rule" if rule.is_none() => rule = Some(value.trim().to_owned()),
                    "fallback_reason" => fallback = true,
                    _ => {}
                }
            }
        }
        // A default for a known agent with no matching rule is not screen evidence.
        let matched = !fallback && rule.as_deref().is_some_and(|rule| rule != "none");
        let state = state
            .filter(|state| {
                matched
                    && matches!(
                        state.as_str(),
                        "idle" | "working" | "blocked" | "done" | "unknown"
                    )
            })
            .unwrap_or_else(|| "unknown".to_owned());
        Ok((agent.filter(|agent| !agent.is_empty()), state))
    }

    /// Label one exact pane as a harness kind with a lifecycle state, without naming an agent.
    pub fn report_pane_agent(&self, pane_id: &str, kind: &str, state: &str) -> Result<()> {
        self.call_ok(
            &strings(&[
                "pane",
                "report-agent",
                pane_id,
                "--source",
                "agentctl",
                "--agent",
                kind,
                "--state",
                state,
                "--message",
                "agentctl custom harness",
            ]),
            &format!("report custom agent {pane_id}"),
        )
    }

    /// Run a slot-boxed harness behind a root relay in a fresh pane and prove it ready.
    ///
    /// Herdr cannot start an agent there (sudo, not a shell, is the pane's foreground
    /// process), so the boxed harness line is run directly, the relayed program is pinned,
    /// and readiness comes from Herdr's live screen rules, which Herdr applies once the pane
    /// is reported as `kind`.
    pub fn start_relay_agent(
        &self,
        kind: &str,
        pane_id: &str,
        command_line: &str,
        timeout: Duration,
        persist_identity: &mut dyn FnMut(CustomProcessIdentity) -> Result<()>,
    ) -> Result<CustomProcessIdentity> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(AdapterError::unavailable(
                "agent startup timeout must be between 0 and 300 seconds",
            ));
        }
        self.call_ok(
            &[
                "pane".to_owned(),
                "run".to_owned(),
                pane_id.to_owned(),
                command_line.to_owned(),
            ],
            &format!("pane run slot relay {kind:?}"),
        )?;
        let deadline = Instant::now() + timeout;
        let mut observed = None;
        while observed.is_none() && Instant::now() < deadline {
            observed = self.relay_process(pane_id).ok().flatten();
            if observed.is_none() {
                thread::sleep(
                    Duration::from_millis(100)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
        let observed = observed.ok_or_else(|| {
            AdapterError::unavailable(format!(
                "no slot relay program appeared in pane {pane_id}; inspect the pane for the wrkslots error"
            ))
        })?;
        persist_identity(observed.clone())?;
        self.report_pane_agent(pane_id, kind, "unknown")?;
        let mut state = "unknown".to_owned();
        loop {
            if Instant::now() >= deadline {
                return Err(AdapterError::unavailable(format!(
                    "{kind} behind the slot relay did not reach an idle screen in pane {pane_id} within {}s (last state {state})",
                    timeout.as_secs_f64()
                )));
            }
            // The program may exec while it starts; any relay program still running counts.
            if self.relay_process(pane_id)?.is_none() {
                return Err(AdapterError::unavailable(format!(
                    "the slot relay program in pane {pane_id} exited during startup"
                )));
            }
            if relay_trust_prompt(&self.read(pane_id, "visible", Some(200))?) {
                return Err(AdapterError::unavailable(format!(
                    "{kind} workspace trust prompt requires human attention; no input was submitted"
                )));
            }
            let (agent, observed_state) = self.explain_agent(pane_id)?;
            state = observed_state;
            // Herdr's title rule can read idle before the harness has drawn anything; the
            // verified composer on screen is the ready signal.
            if agent.as_deref() == Some(kind)
                && state == "idle"
                && crate::submission::composer_view(
                    kind,
                    &self.read_screen_with_cancellation(pane_id, &|| false)?,
                )
                .is_some()
            {
                break;
            }
            if agent.as_deref() == Some(kind) && state == "blocked" {
                return Err(AdapterError::unavailable(format!(
                    "{kind} is waiting for human attention (for example a workspace trust prompt); no input was submitted"
                )));
            }
            thread::sleep(
                Duration::from_millis(200).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        // Pin the generation that reached the idle screen.
        let settled = self.relay_process(pane_id)?.ok_or_else(|| {
            AdapterError::unavailable(format!(
                "the slot relay program in pane {pane_id} exited during startup"
            ))
        })?;
        if settled != observed {
            persist_identity(settled.clone())?;
        }
        self.report_pane_agent(pane_id, kind, "idle")?;
        Ok(settled)
    }

    /// Report whether a pinned command process is still the pane's foreground process.
    pub fn pane_runs_command(
        &self,
        pane_id: &str,
        identity: &CustomProcessIdentity,
    ) -> Result<bool> {
        self.recorded_custom_harness_observed(pane_id, identity, &|| false)
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn agent_pane(&self, name: &str) -> Result<String> {
        self.agent_pane_with_cancellation(name, &|| false)
    }
    pub(crate) fn agent_pane_with_cancellation(
        &self,
        name: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String> {
        let result = self.call_with_cancellation(
            &strings(&["agent", "get", name]),
            &format!("agent get {name:?}"),
            cancelled,
        )?;
        let info = required_object(&result, "agent", "agent get")?;
        if required_string(info, "name", "agent get")? != name {
            return Err(AdapterError::unavailable(format!(
                "agent get: returned a different agent name for {name:?}"
            )));
        }
        required_string(info, "pane_id", "agent get")
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn report_agent_session(
        &self,
        name: &str,
        pane_id: &str,
        kind: &str,
        session_id: &str,
    ) -> Result<()> {
        if self.agent_pane(name)? != pane_id {
            return Err(AdapterError::unavailable(
                "cannot bind a session to a different named agent pane",
            ));
        }
        self.call_ok(
            &strings(&[
                "pane",
                "report-agent-session",
                pane_id,
                "--source",
                "herdr-agent",
                "--agent",
                kind,
                "--agent-session-id",
                session_id,
            ]),
            "report managed agent session",
        )
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn panes(&self, workspace_id: Option<&str>) -> Result<Vec<Pane>> {
        self.panes_with_cancellation(workspace_id, &|| false)
    }
    /// Move one pane into a freshly labelled tab in an existing workspace.
    ///
    /// Herdr assigns a new public pane ID for a cross-workspace move. Validate
    /// both sides of that transition before returning the identity a caller
    /// will persist.
    pub fn move_pane_to_new_tab(
        &self,
        pane_id: &str,
        workspace_id: &str,
        tab_label: &str,
    ) -> Result<Pane> {
        let result = self.call(
            &strings(&[
                "pane",
                "move",
                pane_id,
                "--new-tab",
                "--workspace",
                workspace_id,
                "--label",
                tab_label,
                "--no-focus",
            ]),
            &format!("pane move {pane_id}"),
        )?;
        let moved = required_object(&result, "move_result", "pane move")?;
        if moved.get("changed").and_then(Value::as_bool) != Some(true) {
            return Err(AdapterError::unavailable(
                "pane move: Herdr reported that no move occurred",
            ));
        }
        if required_string(moved, "previous_pane_id", "pane move")? != pane_id {
            return Err(AdapterError::unavailable(
                "pane move: previous pane identity did not match the request",
            ));
        }
        let pane = required_object(moved, "pane", "pane move")?;
        let parsed = Pane {
            pane_id: required_string(pane, "pane_id", "pane move")?,
            tab_id: required_string(pane, "tab_id", "pane move")?,
            workspace_id: required_string(pane, "workspace_id", "pane move")?,
        };
        if parsed.workspace_id != workspace_id {
            return Err(AdapterError::unavailable(format!(
                "pane move: returned workspace {:?}, expected {workspace_id:?}",
                parsed.workspace_id
            )));
        }
        Ok(parsed)
    }
    pub(crate) fn panes_with_cancellation(
        &self,
        workspace_id: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<Pane>> {
        let mut args = strings(&["pane", "list"]);
        if let Some(workspace_id) = workspace_id {
            args.extend(strings(&["--workspace", workspace_id]));
        }
        let result = self.call_with_cancellation(&args, "pane list", cancelled)?;
        let values = value_array(result.get("panes"), "pane list.panes")?;
        let entries = object_entries(values, "pane list entry")?;
        let panes = entries
            .iter()
            .map(|pane| {
                Ok(Pane {
                    pane_id: required_string(pane, "pane_id", "pane list entry")?,
                    tab_id: required_string(pane, "tab_id", "pane list entry")?,
                    workspace_id: required_string(pane, "workspace_id", "pane list entry")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if let Some(expected) = workspace_id {
            if let Some(pane) = panes.iter().find(|pane| pane.workspace_id != expected) {
                return Err(AdapterError::unavailable(format!(
                    "pane list: returned pane {:?} from workspace {:?}, expected {expected:?}",
                    pane.pane_id, pane.workspace_id
                )));
            }
        }
        Ok(panes)
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn pane_info(&self, pane_id: &str) -> Result<AgentPaneInfo> {
        self.pane_info_with_cancellation(pane_id, &|| false)
    }
    pub(crate) fn pane_info_with_cancellation(
        &self,
        pane_id: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<AgentPaneInfo> {
        let result = self.call_with_cancellation(
            &strings(&["pane", "get", pane_id]),
            &format!("pane get {pane_id}"),
            cancelled,
        )?;
        parse_pane_info(&result, pane_id)
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn workspace_label(&self, workspace_id: &str) -> Result<String> {
        self.workspace_label_with_cancellation(workspace_id, &|| false)
    }
    pub(crate) fn workspace_label_with_cancellation(
        &self,
        workspace_id: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String> {
        let result = self.call_with_cancellation(
            &strings(&["workspace", "get", workspace_id]),
            &format!("workspace get {workspace_id}"),
            cancelled,
        )?;
        let workspace = required_object(&result, "workspace", "workspace get")?;
        let returned = required_string(workspace, "workspace_id", "workspace get")?;
        if returned != workspace_id {
            return Err(AdapterError::unavailable(format!(
                "workspace get: returned workspace {returned:?}, expected {workspace_id:?}"
            )));
        }
        required_string(workspace, "label", "workspace get")
    }
    /// Resolve the compatible running server's absolute event-socket path.
    pub fn event_socket(&self) -> Result<PathBuf> {
        self.event_socket_with_cancellation(&|| false)
    }
    pub(crate) fn event_socket_with_cancellation(
        &self,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PathBuf> {
        let completed = self.invoke_with_timeout_and_cancellation(
            &strings(&["status", "server", "--json"]),
            CONTROL_TIMEOUT,
            cancelled,
        )?;
        if completed.status != 0 {
            return Err(AdapterError::unavailable(format!(
                "cannot discover the running Herdr server: {}",
                stderr_detail(&completed)
            )));
        }
        let document = serde_json::from_str::<Value>(&completed.stdout).map_err(|error| {
            AdapterError::unavailable(format!("invalid Herdr server status: {error}"))
        })?;
        let status = document.as_object().ok_or_else(|| {
            AdapterError::unavailable("invalid Herdr server status: expected an object")
        })?;
        if status.get("running").and_then(Value::as_bool) != Some(true)
            || status.get("compatible").and_then(Value::as_bool) != Some(true)
        {
            return Err(AdapterError::unavailable(
                "output subscriptions require a running, compatible Herdr server",
            ));
        }
        let socket = required_string(status, "socket", "Herdr server status")?;
        let socket = PathBuf::from(socket);
        if !socket.is_absolute() || socket.as_os_str().as_bytes().contains(&0) {
            return Err(AdapterError::unavailable(
                "Herdr event socket must be an absolute NUL-free path",
            ));
        }
        Ok(socket)
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn wait_agent_status(&self, pane_id: &str, status: &str, timeout_ms: u64) -> Result<()> {
        self.wait_agent_status_with_cancellation(pane_id, status, timeout_ms, &|| false)
    }
    pub(crate) fn wait_agent_status_with_cancellation(
        &self,
        pane_id: &str,
        status: &str,
        timeout_ms: u64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        let purpose = format!("wait for pane {pane_id} status {status}");
        let completed = self.invoke_with_timeout_and_cancellation(
            &[
                "agent".to_owned(),
                "wait".to_owned(),
                pane_id.to_owned(),
                "--until".to_owned(),
                status.to_owned(),
                "--timeout".to_owned(),
                timeout_ms.to_string(),
            ],
            CONTROL_TIMEOUT
                .max(Duration::from_millis(timeout_ms).saturating_add(Duration::from_secs(5))),
            cancelled,
        )?;
        if completed.status != 0 {
            return Err(AdapterError::unavailable(format!(
                "{purpose}: {}",
                stderr_detail(&completed)
            )));
        }
        let document = serde_json::from_str::<Value>(&completed.stdout).map_err(|error| {
            AdapterError::unavailable(format!("{purpose}: invalid Herdr event response: {error}"))
        })?;
        let envelope = document.as_object().ok_or_else(|| {
            AdapterError::unavailable(format!("{purpose}: event response is not an object"))
        })?;
        let result = required_object(envelope, "result", &purpose)?;
        let data = required_object(result, "agent", &purpose)?;
        let returned_pane = required_string(data, "pane_id", &purpose)?;
        let returned_status = required_string(data, "agent_status", &purpose)?;
        if returned_pane != pane_id || returned_status != status {
            return Err(AdapterError::unavailable(format!(
                "{purpose}: event reported pane={returned_pane:?} status={returned_status:?}"
            )));
        }
        Ok(())
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn read(&self, pane_id: &str, source: &str, lines: Option<usize>) -> Result<String> {
        self.read_with_cancellation(pane_id, source, lines, &|| false)
    }
    pub(crate) fn read_with_cancellation(
        &self,
        pane_id: &str,
        source: &str,
        lines: Option<usize>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String> {
        let mut args = strings(&["pane", "read", pane_id, "--source", source]);
        if let Some(lines) = lines {
            args.extend(["--lines".to_owned(), lines.to_string()]);
        }
        let completed =
            self.invoke_with_timeout_and_cancellation(&args, CONTROL_TIMEOUT, cancelled)?;
        if completed.status != 0 {
            return Err(AdapterError::unavailable(format!(
                "pane read {pane_id}: {}",
                stderr_detail(&completed)
            )));
        }
        Ok(completed.stdout)
    }
    /// Read the visible rows with SGR styling retained for composer inspection.
    pub(crate) fn read_screen_with_cancellation(
        &self,
        pane_id: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String> {
        let lines = crate::submission::SCREEN_LINES.to_string();
        let args = strings(&[
            "pane", "read", pane_id, "--source", "visible", "--lines", &lines, "--format", "ansi",
        ]);
        let completed =
            self.invoke_with_timeout_and_cancellation(&args, CONTROL_TIMEOUT, cancelled)?;
        if completed.status != 0 {
            return Err(AdapterError::unavailable(format!(
                "pane read {pane_id}: {}",
                stderr_detail(&completed)
            )));
        }
        Ok(completed.stdout)
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn prompt_agent(&self, pane_id: &str, text: &str) -> Result<()> {
        self.prompt_agent_with_cancellation(pane_id, text, &|| false)
    }
    /// Insert literal text into one pane without submitting it.
    pub fn send_text(&self, pane_id: &str, text: &str) -> Result<()> {
        self.send_text_with_cancellation(pane_id, text, &|| false)
    }
    pub(crate) fn send_text_with_cancellation(
        &self,
        pane_id: &str,
        text: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        self.send_text_expecting(pane_id, text, None, cancelled)
    }
    /// Insert literal text; with `expect_terminal` the server writes nothing unless the pane
    /// still holds that terminal, and a refusal is an expectation-failed error.
    pub(crate) fn send_text_expecting(
        &self,
        pane_id: &str,
        text: &str,
        expect_terminal: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        self.input_ok(
            strings(&["pane", "send-text", pane_id, text]),
            &format!("pane send-text {pane_id}"),
            expect_terminal,
            cancelled,
        )
    }
    pub(crate) fn prompt_agent_with_cancellation(
        &self,
        pane_id: &str,
        text: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        self.agent_prompt(pane_id, text, None, cancelled)
    }
    /// Submit text plus Enter through Herdr's native prompt primitive, optionally refused by
    /// the server unless the pane still holds `expect_terminal`.
    pub(crate) fn agent_prompt(
        &self,
        pane_id: &str,
        text: &str,
        expect_terminal: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        self.input_ok(
            strings(&["agent", "prompt", pane_id, text]),
            &format!("agent prompt {pane_id}"),
            expect_terminal,
            cancelled,
        )
    }
    /// Run one input command, inserting `--expect-terminal ID` after its two subcommand words.
    ///
    /// A nonzero exit whose output carries Herdr's `expectation_failed` code means the server
    /// wrote nothing, which callers must be able to tell apart from an unknown outcome.
    fn input_ok(
        &self,
        mut args: Vec<String>,
        purpose: &str,
        expect_terminal: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        if let Some(terminal) = expect_terminal {
            args.splice(2..2, ["--expect-terminal".to_owned(), terminal.to_owned()]);
        }
        let completed =
            self.invoke_with_timeout_and_cancellation(&args, CONTROL_TIMEOUT, cancelled)?;
        if completed.status == 0 {
            return Ok(());
        }
        let detail = detail(&completed);
        if expect_terminal.is_some() && expectation_failed(&detail) {
            return Err(AdapterError::expectation_failed(format!(
                "{purpose}: {detail}"
            )));
        }
        Err(AdapterError::unavailable(format!("{purpose}: {detail}")))
    }
    /// Does the running server refuse input whose expected terminal does not match?
    ///
    /// Read once from the `capabilities:` line of `herdr status server`. The protocol number
    /// cannot answer this: a server without the capability ignores the field.
    pub fn input_expect_supported(&self) -> Result<bool> {
        if let Some(supported) = self.input_expect.get() {
            return Ok(*supported);
        }
        let completed =
            self.invoke_with_timeout(&strings(&["status", "server"]), CONTROL_TIMEOUT)?;
        let supported = completed.status == 0 && advertises_input_expect(&completed.stdout);
        Ok(*self.input_expect.get_or_init(|| supported))
    }
    /// Resolve an exact live Herdr agent name to its pane, tab and terminal.
    pub fn agent_identity(&self, name: &str) -> Result<AgentIdentity> {
        let result = self.call(
            &strings(&["agent", "get", name]),
            &format!("agent get {name:?}"),
        )?;
        let info = required_object(&result, "agent", "agent get")?;
        if required_string(info, "name", "agent get")? != name {
            return Err(AdapterError::unavailable(format!(
                "agent get: returned a different agent name for {name:?}"
            )));
        }
        Ok(AgentIdentity {
            name: name.to_owned(),
            pane_id: required_string(info, "pane_id", "agent get")?,
            tab_id: optional_string(info, "tab_id", "agent get")?,
            terminal_id: optional_string(info, "terminal_id", "agent get")?,
        })
    }
    /// Give the agent in one exact pane a new Herdr agent name.
    pub fn rename_agent(&self, pane_id: &str, name: &str) -> Result<()> {
        self.call_ok(
            &strings(&["agent", "rename", pane_id, name]),
            &format!("agent rename {pane_id}"),
        )
    }
    /// Every named Herdr agent, mapped to its pane.
    pub fn agent_names(&self) -> Result<BTreeMap<String, String>> {
        let result = self.call(&strings(&["agent", "list"]), "agent list")?;
        let values = value_array(result.get("agents"), "agent list")?;
        let mut names = BTreeMap::new();
        for entry in object_entries(values, "agent list entry")? {
            if let Some(name) = optional_string(&entry, "name", "agent list entry")? {
                names.insert(
                    name,
                    required_string(&entry, "pane_id", "agent list entry")?,
                );
            }
        }
        Ok(names)
    }
    /// Every tab of one workspace, mapped to its label.
    pub fn tab_labels(&self, workspace_id: &str) -> Result<BTreeMap<String, String>> {
        let result = self.call(
            &strings(&["tab", "list", "--workspace", workspace_id]),
            "tab list",
        )?;
        let values = value_array(result.get("tabs"), "tab list")?;
        let mut labels = BTreeMap::new();
        for entry in object_entries(values, "tab list entry")? {
            labels.insert(
                required_string(&entry, "tab_id", "tab list entry")?,
                required_string(&entry, "label", "tab list entry")?,
            );
        }
        Ok(labels)
    }
    /// Return the label of one exact tab.
    pub fn tab_label(&self, tab_id: &str) -> Result<String> {
        let result = self.call(
            &strings(&["tab", "get", tab_id]),
            &format!("tab get {tab_id}"),
        )?;
        let tab = required_object(&result, "tab", "tab get")?;
        if required_string(tab, "tab_id", "tab get")? != tab_id {
            return Err(AdapterError::unavailable(format!(
                "tab get: returned a different tab for {tab_id:?}"
            )));
        }
        required_string(tab, "label", "tab get")
    }
    /// Inspect a recorded local process without confusing probe failure with death.
    ///
    /// A changed boot or process generation, or a positive kernel exit indication,
    /// establishes death. A different live executable image remains unknown. This
    /// proof does not authorize input or a signal without separate pane ownership.
    pub fn process_liveness(&self, expected: &CustomProcessIdentity) -> ProcessLiveness {
        #[cfg(target_os = "linux")]
        {
            process_liveness_with_probe(expected, &mut KernelProcessLivenessProbe)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = expected;
            ProcessLiveness::Unknown
        }
    }

    /// Pin the pane's foreground process-group leader, the harness Herdr detected.
    ///
    /// Returns `None` when the leader is the pane's shell, is not listed among the foreground
    /// Pin the pane's harness process when it is the only foreground process.
    ///
    /// A foreground group with several processes (a launcher and the program it started, or
    /// the harness running a foreground child) is ambiguous: the pinned process could be a
    /// launcher that outlives the harness. `None` then, and when the kernel identity cannot be
    /// read coherently. `kind` is the expected harness.
    pub fn harness_identity(
        &self,
        pane_id: &str,
        kind: &str,
    ) -> Result<Option<CustomProcessIdentity>> {
        self.harness_identity_with_cancellation(pane_id, kind, &|| false)
    }

    fn harness_identity_with_cancellation(
        &self,
        pane_id: &str,
        kind: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<CustomProcessIdentity>> {
        let _ = kind;
        let state = self.pane_process_state(pane_id, cancelled)?;
        if state.foreground_process_group_id == state.shell_pid || state.processes.len() != 1 {
            return Ok(None);
        }
        let Some(pid) = state.processes[0].get("pid").and_then(Value::as_u64) else {
            return Ok(None);
        };
        let Ok(observed) = live_custom_process(pid) else {
            return Ok(None);
        };
        Ok(
            (observed.process_group_id == state.foreground_process_group_id)
                .then_some(observed.identity),
        )
    }
    /// Is the pinned harness process, the same kernel lifetime, still the pane's only
    /// foreground process? (An anchor pinned to a launcher therefore fails.)
    pub fn verify_harness_identity(
        &self,
        pane_id: &str,
        expected: &CustomProcessIdentity,
    ) -> Result<bool> {
        self.verify_harness_identity_with_cancellation(pane_id, expected, &|| false)
    }

    fn verify_harness_identity_with_cancellation(
        &self,
        pane_id: &str,
        expected: &CustomProcessIdentity,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<bool> {
        let state = self.pane_process_state(pane_id, cancelled)?;
        if !expected.valid() || state.processes.len() != 1 {
            return Ok(false);
        }
        if state.processes[0].get("pid").and_then(Value::as_u64) != Some(expected.pid) {
            return Ok(false);
        }
        let Ok(observed) = live_custom_process(expected.pid) else {
            return Ok(false);
        };
        Ok(
            observed.process_group_id == state.foreground_process_group_id
                && observed.identity == *expected,
        )
    }

    pub(crate) fn codex_idle_ready_with_cancellation(
        &self,
        info: &AgentPaneInfo,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<bool> {
        let native_valid = match (&info.session_agent, &info.session_value) {
            (None, None) => true,
            (Some(agent), Some(value)) => {
                agent == "codex" && !value.is_empty() && !value.contains('\0')
            }
            _ => false,
        };
        if info.agent.as_deref() != Some("codex")
            || info.status != "unknown"
            || !native_valid
            || info
                .terminal_id
                .as_deref()
                .is_none_or(|value| value.is_empty() || value.contains('\0'))
        {
            return Ok(false);
        }
        let Some(identity) =
            self.harness_identity_with_cancellation(&info.pane_id, "codex", cancelled)?
        else {
            return Ok(false);
        };
        let screen = self.read_screen_with_cancellation(&info.pane_id, cancelled)?;
        if !crate::submission::codex_idle_composer(&screen) {
            return Ok(false);
        }
        let confirmed = self.pane_info_with_cancellation(&info.pane_id, cancelled)?;
        Ok(confirmed == *info
            && self.verify_harness_identity_with_cancellation(
                &info.pane_id,
                &identity,
                cancelled,
            )?)
    }
    /// Invoke and validate the corresponding Herdr agent-control operation.
    pub fn send_keys(&self, pane_id: &str, keys: &str) -> Result<()> {
        self.send_keys_with_cancellation(pane_id, keys, &|| false)
    }
    pub(crate) fn send_keys_with_cancellation(
        &self,
        pane_id: &str,
        keys: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        self.send_keys_expecting(pane_id, keys, None, cancelled)
    }
    /// Send named keys, optionally refused by the server unless the pane still holds
    /// `expect_terminal`.
    pub(crate) fn send_keys_expecting(
        &self,
        pane_id: &str,
        keys: &str,
        expect_terminal: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        self.input_ok(
            strings(&["pane", "send-keys", pane_id, keys]),
            &format!("pane send-keys {pane_id}"),
            expect_terminal,
            cancelled,
        )
    }
    fn workspace_entries(&self) -> Result<Vec<Map<String, Value>>> {
        let result = self.call(&strings(&["workspace", "list"]), "workspace list")?;
        let values = value_array(result.get("workspaces"), "workspace list.workspaces")?;
        object_entries(values, "workspace list entry")
    }
    fn call(&self, args: &[String], purpose: &str) -> Result<Map<String, Value>> {
        self.call_with_cancellation(args, purpose, &|| false)
    }
    fn call_with_cancellation(
        &self,
        args: &[String],
        purpose: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Map<String, Value>> {
        let completed =
            self.invoke_with_timeout_and_cancellation(args, CONTROL_TIMEOUT, cancelled)?;
        if completed.status != 0 {
            return Err(AdapterError::unavailable(format!(
                "{purpose}: {}",
                detail(&completed)
            )));
        }
        let document = serde_json::from_str::<Value>(&completed.stdout).map_err(|_| {
            let preview: String = completed.stdout.trim().chars().take(200).collect();
            AdapterError::unavailable(format!(
                "{purpose}: herdr returned non-JSON output: {preview:?}"
            ))
        })?;
        let envelope = document.as_object().ok_or_else(|| {
            AdapterError::unavailable(format!("{purpose}: Herdr response is not an object"))
        })?;
        envelope
            .get("result")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| {
                AdapterError::unavailable(format!("{purpose}: Herdr response has no result object"))
            })
    }
    fn call_ok(&self, args: &[String], purpose: &str) -> Result<()> {
        self.call_ok_with_cancellation(args, purpose, &|| false)
    }
    fn call_ok_with_cancellation(
        &self,
        args: &[String],
        purpose: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        let completed =
            self.invoke_with_timeout_and_cancellation(args, CONTROL_TIMEOUT, cancelled)?;
        if completed.status == 0 {
            Ok(())
        } else {
            Err(AdapterError::unavailable(format!(
                "{purpose}: {}",
                detail(&completed)
            )))
        }
    }
    /// Create a tab and capture its initial pane from the same allocation result.
    pub fn create_tab_with_pane(
        &self,
        workspace: &str,
        label: &str,
        cwd: &str,
        environment: &[String],
    ) -> Result<(String, String)> {
        let arguments = tab_create_arguments(workspace, label, cwd, environment);
        let result = self.call(&arguments, "tab create")?;
        let tab = required_object(&result, "tab", "tab create")?;
        let pane = required_object(&result, "root_pane", "tab create")?;
        let tab_id = required_string(tab, "tab_id", "tab create")?;
        if required_string(pane, "tab_id", "created pane")? != tab_id
            || required_string(pane, "workspace_id", "created pane")? != workspace
        {
            return Err(AdapterError::unavailable(
                "created pane does not belong to the allocated tab/workspace",
            ));
        }
        Ok((tab_id, required_string(pane, "pane_id", "tab create")?))
    }
    /// Close exactly the owned pane; concurrent human-created sibling panes survive.
    pub fn close_pane(&self, pane: &str) -> Result<()> {
        self.call_ok(&strings(&["pane", "close", pane]), "pane close")
    }
    /// Focus the named pane in its existing Herdr workspace.
    pub fn focus_pane(&self, pane: &str) -> Result<()> {
        self.call_ok(&strings(&["agent", "focus", pane]), "agent focus")
    }
}

fn workspace_create_arguments(label: &str, cwd: &str, environment: &[String]) -> Vec<String> {
    let mut arguments = strings(&["workspace", "create", "--label", label, "--cwd", cwd]);
    append_environment_and_no_focus(&mut arguments, environment);
    arguments
}

fn tab_create_arguments(
    workspace: &str,
    label: &str,
    cwd: &str,
    environment: &[String],
) -> Vec<String> {
    let mut arguments = strings(&[
        "tab",
        "create",
        "--workspace",
        workspace,
        "--label",
        label,
        "--cwd",
        cwd,
    ]);
    append_environment_and_no_focus(&mut arguments, environment);
    arguments
}

fn append_environment_and_no_focus(arguments: &mut Vec<String>, environment: &[String]) {
    for entry in environment {
        arguments.extend(["--env".to_owned(), entry.clone()]);
    }
    arguments.push("--no-focus".to_owned());
}

fn parse_pane_info(result: &Map<String, Value>, pane_id: &str) -> Result<AgentPaneInfo> {
    let pane = required_object(result, "pane", "pane get")?;
    let returned = required_string(pane, "pane_id", "pane get")?;
    if returned != pane_id {
        return Err(AdapterError::unavailable(format!(
            "pane get: returned pane {returned:?}, expected {pane_id:?}"
        )));
    }
    let (session_agent, session_value) = match pane.get("agent_session") {
        None | Some(Value::Null) => (None, None),
        Some(Value::Object(session)) => (
            optional_string(session, "agent", "pane agent_session")?,
            optional_string(session, "value", "pane agent_session")?,
        ),
        Some(_) => {
            return Err(AdapterError::unavailable(
                "pane get: 'agent_session' is not an object",
            ));
        }
    };
    let scroll = match pane.get("scroll") {
        None | Some(Value::Null) => None,
        Some(Value::Object(scroll)) => Some(PaneScroll {
            offset_from_bottom: required_u64(scroll, "offset_from_bottom", "pane scroll")?,
            max_offset_from_bottom: required_u64(scroll, "max_offset_from_bottom", "pane scroll")?,
            viewport_rows: required_u64(scroll, "viewport_rows", "pane scroll")?,
        }),
        Some(_) => {
            return Err(AdapterError::unavailable(
                "pane get: 'scroll' is not an object",
            ));
        }
    };
    Ok(AgentPaneInfo {
        pane_id: returned,
        workspace_id: required_string(pane, "workspace_id", "pane get")?,
        cwd: required_string(pane, "cwd", "pane get")?,
        agent: optional_string(pane, "agent", "pane get")?,
        status: optional_string(pane, "agent_status", "pane get")?
            .unwrap_or_else(|| "unknown".to_owned()),
        session_agent,
        session_value,
        scroll,
        terminal_id: optional_string(pane, "terminal_id", "pane get")?,
        tab_id: optional_string(pane, "tab_id", "pane get")?,
    })
}

fn resolve_executable(configured: &Path) -> Result<PathBuf> {
    let candidates = if configured.components().count() > 1 || configured.is_absolute() {
        vec![configured.to_owned()]
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(configured))
            .collect()
    };
    for path in candidates {
        if fs::metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        {
            return fs::canonicalize(&path).map_err(|error| {
                AdapterError::unavailable(format!("cannot resolve Herdr executable: {error}"))
            });
        }
    }
    Err(AdapterError::unavailable(format!(
        "Herdr executable not found: {}; install Herdr or pass --herdr-bin",
        configured.display()
    )))
}

pub(crate) fn account_home() -> Result<PathBuf> {
    // SAFETY: getpwuid_r writes only into the supplied record and byte buffer;
    // the C string is copied before either backing allocation is dropped.
    let uid = unsafe { libc::getuid() };
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut size = if suggested > 0 {
        usize::try_from(suggested).unwrap_or(16_384)
    } else {
        16_384
    };
    loop {
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; size];
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if code == libc::ERANGE {
            size = size.saturating_mul(2);
            if size > 16 * 1024 * 1024 {
                return Err(AdapterError::unavailable(
                    "cannot resolve current account home: account record is too large",
                ));
            }
            continue;
        }
        if code != 0 {
            return Err(AdapterError::unavailable(format!(
                "cannot resolve current account home: {}",
                io::Error::from_raw_os_error(code)
            )));
        }
        if result.is_null() {
            return Err(AdapterError::unavailable(
                "cannot resolve current account home: no account record",
            ));
        }
        let record = unsafe { record.assume_init() };
        if record.pw_dir.is_null() {
            return Err(AdapterError::unavailable(
                "the current account has no home directory",
            ));
        }
        let bytes = unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes();
        if bytes.is_empty() {
            return Err(AdapterError::unavailable(
                "the current account has no home directory",
            ));
        }
        return Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())));
    }
}

pub(crate) fn resolve_harness_executable(kind: &str) -> Result<PathBuf> {
    if kind.is_empty()
        || !kind.as_bytes()[0].is_ascii_lowercase()
        || !kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(AdapterError::unavailable(
            "custom harness name must contain lowercase ASCII letters, digits, or hyphens",
        ));
    }
    if kind == "muse" {
        if let Some(configured) = std::env::var_os("AGENTCTL_MUSE_BIN") {
            let path = PathBuf::from(configured);
            if !path.is_absolute() {
                return Err(AdapterError::unavailable(
                    "AGENTCTL_MUSE_BIN must be an absolute path",
                ));
            }
            let resolved = fs::canonicalize(&path).map_err(|error| {
                AdapterError::unavailable(format!(
                    "cannot inspect muse executable {}: {error}",
                    path.display()
                ))
            })?;
            let metadata = fs::metadata(&resolved).map_err(|error| {
                AdapterError::unavailable(format!(
                    "cannot inspect muse executable {}: {error}",
                    resolved.display()
                ))
            })?;
            if !metadata.is_file()
                || metadata.permissions().mode() & 0o111 == 0
                || metadata.permissions().mode() & 0o022 != 0
            {
                return Err(AdapterError::unavailable(format!(
                    "refusing unsafe muse executable: {}",
                    resolved.display()
                )));
            }
            return Ok(resolved);
        }
    }
    let home = account_home()?;
    let candidates = [
        PathBuf::from("/usr/local/bin").join(kind),
        PathBuf::from("/usr/bin").join(kind),
        home.join(".local/bin").join(kind),
        home.join("bin").join(kind),
        home.join(".cargo/bin").join(kind),
    ];
    for path in candidates {
        if fs::metadata(&path).is_ok_and(|metadata| {
            metadata.is_file()
                && metadata.permissions().mode() & 0o111 != 0
                && metadata.permissions().mode() & 0o022 == 0
        }) {
            return fs::canonicalize(&path).map_err(|error| {
                AdapterError::unavailable(format!(
                    "cannot resolve custom harness executable {kind:?}: {error}"
                ))
            });
        }
    }
    Err(AdapterError::unavailable(format!(
        "custom harness executable {kind:?} was not found in fixed install locations"
    )))
}

fn shell_join(arguments: &[String]) -> Result<String> {
    let mut quoted = Vec::with_capacity(arguments.len());
    for value in arguments {
        if value.contains('\0') {
            return Err(AdapterError::unavailable(
                "harness arguments must contain no NUL",
            ));
        }
        if !value.is_empty()
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'=' | b',' | b'+')
            })
        {
            quoted.push(value.clone());
        } else {
            quoted.push(format!("'{}'", value.replace('\'', "'\\''")));
        }
    }
    Ok(quoted.join(" "))
}
fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn detail(output: &CommandOutput) -> String {
    let detail = if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    };
    if detail.is_empty() {
        format!("exit {}", output.status)
    } else {
        detail.to_owned()
    }
}

/// The `code` of a Herdr error object quoted in `detail`, such as `pane_not_found`.
pub(crate) fn herdr_error_code(detail: &str) -> Option<String> {
    let compact: String = detail
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let (_, rest) = compact.split_once("\"code\":\"")?;
    rest.split_once('"').map(|(code, _)| code.to_owned())
}

/// Whether a refused input command reported Herdr's `expectation_failed` error code.
fn expectation_failed(detail: &str) -> bool {
    detail
        .chars()
        .filter(|character| *character != ' ')
        .collect::<String>()
        .contains("\"code\":\"expectation_failed\"")
}

/// Whether `herdr status server` output lists `input-expect` on its `capabilities:` line.
fn advertises_input_expect(status: &str) -> bool {
    let mut supported = false;
    for line in status.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if key.trim() == "capabilities" {
                supported = value
                    .replace(',', " ")
                    .split_whitespace()
                    .any(|token| token == "input-expect");
            }
        }
    }
    supported
}

fn stderr_detail(output: &CommandOutput) -> String {
    let detail = output.stderr.trim();
    if detail.is_empty() {
        format!("exit {}", output.status)
    } else {
        detail.to_owned()
    }
}

fn value_array<'a>(value: Option<&'a Value>, what: &str) -> Result<&'a [Value]> {
    match value {
        Some(Value::Array(values)) => Ok(values),
        None | Some(_) => Err(AdapterError::unavailable(format!(
            "{what}: expected an array"
        ))),
    }
}

fn object_entries(values: &[Value], what: &str) -> Result<Vec<Map<String, Value>>> {
    values
        .iter()
        .map(|value| {
            value
                .as_object()
                .cloned()
                .ok_or_else(|| AdapterError::unavailable(format!("{what}: expected an object")))
        })
        .collect()
}

fn required_object<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<&'a Map<String, Value>> {
    object
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| AdapterError::unavailable(format!("{what}: {key:?} is not an object")))
}

fn required_string(object: &Map<String, Value>, key: &str, what: &str) -> Result<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| AdapterError::unavailable(format!("{what}: {key:?} is not a string")))
}

fn required_u64(object: &Map<String, Value>, key: &str, what: &str) -> Result<u64> {
    object.get(key).and_then(Value::as_u64).ok_or_else(|| {
        AdapterError::unavailable(format!("{what}: {key:?} is not a non-negative integer"))
    })
}

fn optional_string(object: &Map<String, Value>, key: &str, what: &str) -> Result<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(AdapterError::unavailable(format!(
            "{what}: {key:?} is not a string"
        ))),
    }
}

fn unique_label_id(
    entries: &[Map<String, Value>],
    id_key: &str,
    label: &str,
    kind: &str,
) -> Result<Option<String>> {
    let mut matches = Vec::new();
    for entry in entries {
        if optional_string(entry, "label", &format!("{kind} list entry"))?.as_deref() == Some(label)
        {
            matches.push(required_string(
                entry,
                id_key,
                &format!("{kind} list entry"),
            )?);
        }
    }
    if matches.len() > 1 {
        return Err(AdapterError::unavailable(format!(
            "{kind} label {label:?} is ambiguous: {} matching IDs",
            matches.len()
        )));
    }
    Ok(matches.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    mod herdr_compatibility;
    #[cfg(target_os = "linux")]
    use std::io::Write as _;
    #[cfg(target_os = "linux")]
    use std::process::Child;
    #[cfg(target_os = "linux")]
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    #[cfg(target_os = "linux")]
    use std::sync::Arc;
    #[cfg(target_os = "linux")]
    use std::thread;

    #[cfg(target_os = "linux")]
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    #[cfg(target_os = "linux")]
    static EXECUTABLE_FIXTURE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    #[cfg(target_os = "linux")]
    static SIGNAL_FIXTURE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(target_os = "linux")]
    #[derive(Debug)]
    enum LivenessReply {
        Boot(Result<String>),
        Pin(io::Result<()>),
        Exited(Result<bool>),
        Stat(Result<LinuxProcessStat>),
        Image(Result<(u64, u64)>),
    }

    #[cfg(target_os = "linux")]
    struct ScriptedLivenessProbe(std::collections::VecDeque<LivenessReply>);

    #[cfg(target_os = "linux")]
    impl ScriptedLivenessProbe {
        fn next(&mut self) -> LivenessReply {
            self.0.pop_front().expect("unexpected liveness probe call")
        }
    }

    #[cfg(target_os = "linux")]
    impl ProcessLivenessProbe for ScriptedLivenessProbe {
        type Handle = ();

        fn boot_identity(&mut self) -> Result<String> {
            match self.next() {
                LivenessReply::Boot(value) => value,
                reply => panic!("expected boot probe, got {reply:?}"),
            }
        }

        fn pin(&mut self, pid: u64) -> io::Result<()> {
            assert_eq!(pid, 4242);
            match self.next() {
                LivenessReply::Pin(value) => value,
                reply => panic!("expected pidfd_open, got {reply:?}"),
            }
        }

        fn exited(&mut self, _: &()) -> Result<bool> {
            match self.next() {
                LivenessReply::Exited(value) => value,
                reply => panic!("expected pidfd poll, got {reply:?}"),
            }
        }

        fn process_stat(&mut self, pid: u64) -> Result<LinuxProcessStat> {
            assert_eq!(pid, 4242);
            match self.next() {
                LivenessReply::Stat(value) => value,
                reply => panic!("expected stat probe, got {reply:?}"),
            }
        }

        fn executable_identity(&mut self, pid: u64) -> Result<(u64, u64)> {
            assert_eq!(pid, 4242);
            match self.next() {
                LivenessReply::Image(value) => value,
                reply => panic!("expected image probe, got {reply:?}"),
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn liveness_record() -> CustomProcessIdentity {
        CustomProcessIdentity {
            version: 1,
            boot_id: "00000000-0000-0000-0000-000000000000".to_owned(),
            pid: 4242,
            starttime_ticks: 99,
            executable_device: 10,
            executable_inode: 11,
        }
    }

    #[cfg(target_os = "linux")]
    fn liveness_stat(state: u8, starttime_ticks: u64) -> LinuxProcessStat {
        LinuxProcessStat {
            state,
            parent_pid: 1,
            process_group_id: 4242,
            starttime_ticks,
        }
    }

    #[cfg(target_os = "linux")]
    fn boot_reply() -> LivenessReply {
        LivenessReply::Boot(Ok(liveness_record().boot_id))
    }

    #[cfg(target_os = "linux")]
    fn alive_replies() -> Vec<LivenessReply> {
        vec![
            boot_reply(),
            LivenessReply::Pin(Ok(())),
            LivenessReply::Exited(Ok(false)),
            LivenessReply::Stat(Ok(liveness_stat(b'S', 99))),
            LivenessReply::Image(Ok((10, 11))),
            LivenessReply::Stat(Ok(liveness_stat(b'R', 99))),
            LivenessReply::Image(Ok((10, 11))),
            boot_reply(),
            LivenessReply::Exited(Ok(false)),
        ]
    }

    #[cfg(target_os = "linux")]
    fn inspect_script(
        expected: &CustomProcessIdentity,
        replies: Vec<LivenessReply>,
    ) -> ProcessLiveness {
        process_liveness_with_probe(expected, &mut ScriptedLivenessProbe(replies.into()))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_liveness_tracks_a_real_child_before_and_after_kill() {
        let mut process = spawn_test_process(Path::new("/usr/bin/sleep"));
        let pid = u64::from(process.0.id());
        let identity = live_custom_process(pid).unwrap().identity;
        let herdr = FakeExecutable::new("{}");
        assert_eq!(
            herdr.client().process_liveness(&identity),
            ProcessLiveness::Alive
        );
        let pinned = open_pidfd(pid).unwrap();
        process.0.kill().unwrap();
        let mut descriptor = libc::pollfd {
            fd: pinned.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor is initialized and refers to an owned child's pidfd.
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 2000) }, 1);
        // Observe the unreaped zombie first, then the absent PID after reaping.
        assert_eq!(
            herdr.client().process_liveness(&identity),
            ProcessLiveness::Dead
        );
        process.0.wait().unwrap();
        assert_eq!(
            herdr.client().process_liveness(&identity),
            ProcessLiveness::Dead
        );
        assert!(
            !herdr.root.join("args").exists(),
            "liveness must not invoke Herdr"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_liveness_requires_positive_generation_or_exit_evidence() {
        let expected = liveness_record();
        assert_eq!(
            inspect_script(&expected, alive_replies()),
            ProcessLiveness::Alive
        );
        assert_eq!(
            inspect_script(
                &expected,
                vec![
                    boot_reply(),
                    LivenessReply::Pin(Err(io::Error::from_raw_os_error(libc::ESRCH))),
                    boot_reply(),
                ]
            ),
            ProcessLiveness::Dead
        );
        assert_eq!(
            inspect_script(
                &expected,
                vec![
                    boot_reply(),
                    LivenessReply::Pin(Ok(())),
                    LivenessReply::Exited(Ok(true)),
                    boot_reply(),
                ]
            ),
            ProcessLiveness::Dead
        );
        let mut old_boot = expected.clone();
        old_boot.boot_id = "ffffffff-ffff-ffff-ffff-ffffffffffff".to_owned();
        assert_eq!(
            inspect_script(&old_boot, vec![boot_reply(), boot_reply()]),
            ProcessLiveness::Dead
        );
        for (state, ticks) in [(b'S', 100), (b'Z', 99), (b'X', 99), (b'x', 99)] {
            assert_eq!(
                inspect_script(
                    &expected,
                    vec![
                        boot_reply(),
                        LivenessReply::Pin(Ok(())),
                        LivenessReply::Exited(Ok(false)),
                        LivenessReply::Stat(Ok(liveness_stat(state, ticks))),
                        LivenessReply::Stat(Ok(liveness_stat(state, ticks))),
                        boot_reply(),
                        LivenessReply::Exited(Ok(false)),
                    ]
                ),
                ProcessLiveness::Dead
            );
        }
        for field in ["device", "inode"] {
            let mut different_image = expected.clone();
            if field == "device" {
                different_image.executable_device += 1;
            } else {
                different_image.executable_inode += 1;
            }
            assert_eq!(
                inspect_script(&different_image, alive_replies()),
                ProcessLiveness::Unknown
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_liveness_keeps_failed_and_malformed_probes_unknown() {
        let expected = liveness_record();
        for errno in [
            libc::EPERM,
            libc::EACCES,
            libc::ENOSYS,
            libc::EINVAL,
            libc::EIO,
            libc::EINTR,
        ] {
            let mut replies = alive_replies();
            replies[1] = LivenessReply::Pin(Err(io::Error::from_raw_os_error(errno)));
            assert_eq!(inspect_script(&expected, replies), ProcessLiveness::Unknown);
        }
        for detail in [
            "permission denied",
            "I/O failure",
            "missing proc file",
            "malformed or oversized read",
        ] {
            for index in [0, 2, 3, 4, 5, 6, 7, 8] {
                let mut replies = alive_replies();
                let error = Err(AdapterError::unavailable(detail));
                replies[index] = match index {
                    0 | 7 => LivenessReply::Boot(error),
                    2 | 8 => LivenessReply::Exited(Err(AdapterError::unavailable(detail))),
                    3 | 5 => LivenessReply::Stat(Err(AdapterError::unavailable(detail))),
                    4 | 6 => LivenessReply::Image(Err(AdapterError::unavailable(detail))),
                    _ => unreachable!(),
                };
                assert_eq!(inspect_script(&expected, replies), ProcessLiveness::Unknown);
            }
        }
        for boot in [String::new(), "invalid".to_owned(), "x".repeat(129)] {
            assert_eq!(
                inspect_script(&expected, vec![LivenessReply::Boot(Ok(boot))]),
                ProcessLiveness::Unknown
            );
        }
        for stat in [liveness_stat(b'S', 0), liveness_stat(b'?', 99)] {
            let mut replies = alive_replies();
            replies[3] = LivenessReply::Stat(Ok(stat));
            assert_eq!(inspect_script(&expected, replies), ProcessLiveness::Unknown);
        }
        let mut invalid = expected.clone();
        invalid.version = 0;
        assert_eq!(inspect_script(&invalid, vec![]), ProcessLiveness::Unknown);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_liveness_refuses_changing_boot_stat_or_image_reads() {
        let expected = liveness_record();
        for (index, replacement) in [
            (
                7,
                LivenessReply::Boot(Ok("ffffffff-ffff-ffff-ffff-ffffffffffff".to_owned())),
            ),
            (5, LivenessReply::Stat(Ok(liveness_stat(b'S', 100)))),
            (6, LivenessReply::Image(Ok((10, 12)))),
        ] {
            let mut replies = alive_replies();
            replies[index] = replacement;
            assert_eq!(inspect_script(&expected, replies), ProcessLiveness::Unknown);
        }
        for (first, second) in [
            (liveness_stat(b'S', 100), liveness_stat(b'S', 101)),
            (liveness_stat(b'Z', 99), liveness_stat(b'S', 99)),
        ] {
            assert_eq!(
                inspect_script(
                    &expected,
                    vec![
                        boot_reply(),
                        LivenessReply::Pin(Ok(())),
                        LivenessReply::Exited(Ok(false)),
                        LivenessReply::Stat(Ok(first)),
                        LivenessReply::Stat(Ok(second)),
                        boot_reply(),
                        LivenessReply::Exited(Ok(false)),
                    ]
                ),
                ProcessLiveness::Unknown
            );
        }
        // Positive absence still needs a stable readable boot identity.
        assert_eq!(
            inspect_script(
                &expected,
                vec![
                    boot_reply(),
                    LivenessReply::Pin(Err(io::Error::from_raw_os_error(libc::ESRCH))),
                    LivenessReply::Boot(Err(AdapterError::unavailable("boot unreadable"))),
                ]
            ),
            ProcessLiveness::Unknown
        );
    }

    #[test]
    fn process_stat_parser_treats_the_command_as_opaque_bytes() {
        let mut record = b"4242 (name\xff) Z 999 888\nPPid:\t123 (more)".to_vec();
        record.extend_from_slice(b") S 7 9 11 0 -1 4194304 1 2 3 4 13 14 15 16 20 0 1 0 22 0 0\n");
        let parsed = parse_process_stat(&record, 4242).unwrap();
        assert_eq!(parsed.state, b'S');
        assert_eq!(parsed.parent_pid, 7);
        assert_eq!(parsed.process_group_id, 9);
        assert_eq!(parsed.starttime_ticks, 22);
        assert!(parse_process_stat(&record, 4241).is_err());

        let mut malformed = record;
        malformed.push(b')');
        assert!(parse_process_stat(&malformed, 4242).is_err());

        let mut shifted =
            b"4242 (worker) S 7 9 11 0 -1 4194304 1 2 3 4 13 14 15 16 20 0 1 0 22 0 0".to_vec();
        shifted.extend_from_slice(b") 7 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22");
        assert!(parse_process_stat(&shifted, 4242).is_err());

        for state in b"RSDZTtXxKWPI" {
            let mut value =
                b"4242 (worker) S 7 9 11 0 -1 4194304 1 2 3 4 13 14 15 16 20 0 1 0 22 0 0".to_vec();
            let closing = value.iter().rposition(|byte| *byte == b')').unwrap();
            value[closing + 2] = *state;
            assert_eq!(parse_process_stat(&value, 4242).unwrap().state, *state);
        }
        for state in b"Q0)\x7f" {
            let mut value =
                b"4242 (worker) S 7 9 11 0 -1 4194304 1 2 3 4 13 14 15 16 20 0 1 0 22 0 0".to_vec();
            let closing = value.iter().rposition(|byte| *byte == b')').unwrap();
            value[closing + 2] = *state;
            assert!(parse_process_stat(&value, 4242).is_err());
        }
    }

    #[test]
    fn muse_prompt_must_move_from_composer_to_transcript() {
        let prompt = "literal $(unexpanded) delivery\nsecond line";
        let header = "Muse Code 1.3.0\n";
        let divider = "────────────────\n";
        let footer = "watermelon-preview · xhigh · /work/project · Auto-review\n";
        let staged = format!("{header}{divider}❯ {prompt}\n{divider}{footer}");
        let accepted = format!("{header}❯ {prompt}\nWorking...\n{divider}❯\n{divider}{footer}");
        let error_redraw = format!("{header}{divider}❯ {prompt}\nError: retry\n{divider}{footer}");
        assert!(muse_prompt_in_composer(&staged, prompt));
        assert_eq!(muse_prompt_transcript_count(&staged, prompt), 0);
        assert_eq!(muse_prompt_transcript_count(&accepted, prompt), 1);
        assert!(!muse_prompt_in_composer(&accepted, prompt));
        assert!(muse_prompt_in_composer(&error_redraw, prompt));
        assert_eq!(muse_prompt_transcript_count(&error_redraw, prompt), 0);
        let repeated_staged =
            format!("{header}❯ {prompt}\n◆ prior answer\n{divider}❯ {prompt}\n{divider}{footer}");
        let cleared_without_submit =
            format!("{header}❯ {prompt}\n◆ prior answer\n{divider}❯\n{divider}{footer}");
        assert_eq!(muse_prompt_transcript_count(&repeated_staged, prompt), 1);
        assert_eq!(
            muse_prompt_transcript_count(&cleared_without_submit, prompt),
            1
        );
    }

    #[cfg(target_os = "linux")]
    struct FakeExecutable {
        root: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }
    #[cfg(target_os = "linux")]
    impl FakeExecutable {
        fn new(response: &str) -> Self {
            let guard = EXECUTABLE_FIXTURE.lock().unwrap();
            let root = std::env::temp_dir().join(format!(
                "agentctl-adapter-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let fixture = Self {
                root,
                _guard: guard,
            };
            fixture.set_response(response);
            fixture
        }
        fn set_response(&self, response: &str) {
            let script = format!("#!/usr/bin/python3\nimport json, pathlib, sys\npathlib.Path(__file__).with_name('args').write_text(json.dumps(sys.argv[1:]))\nprint({response:?})\n");
            fs::write(self.root.join("herdr"), script).unwrap();
            fs::set_permissions(self.root.join("herdr"), fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        fn client(&self) -> HerdrClient {
            HerdrClient::with_executable("direct", &self.root.join("herdr")).unwrap()
        }
        fn arguments(&self) -> Value {
            serde_json::from_slice(&fs::read(self.root.join("args")).unwrap()).unwrap()
        }
    }
    #[cfg(target_os = "linux")]
    impl Drop for FakeExecutable {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(target_os = "linux")]
    struct RecordedChild {
        root: PathBuf,
        group_pid_file: PathBuf,
        escaped_pid_file: PathBuf,
    }

    #[cfg(target_os = "linux")]
    impl RecordedChild {
        fn new(kind: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "agentctl-{kind}-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            let group_pid_file = root.join("group-pid");
            let escaped_pid_file = root.join("escaped-pid");
            Self {
                root,
                group_pid_file,
                escaped_pid_file,
            }
        }

        fn escaped_pipe_command(&self) -> Command {
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    // The holder publishes its own pid atomically, so a timeout that kills this
                    // shell cannot truncate the marker, and a forked holder is still recorded.
                    "/usr/bin/setsid /bin/sh -c 'printf \"%s\\n\" \"$$\" > \"$1.tmp\" && mv \"$1.tmp\" \"$1\" && exec /bin/sleep 30' agentctl-holder \"$1\" & exit 0",
                    "agentctl-test",
                ])
                .arg(&self.escaped_pid_file);
            command
        }

        fn mixed_descendant_command(&self) -> Command {
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    // The escaped child, not its parent shell, publishes the PID after setsid.
                    // A parent-side shell PID marker can race cancellation before the escape.
                    "/bin/sleep 30 & printf '%s\\n' \"$!\" > \"$1\"; /usr/bin/setsid /bin/sh -c 'printf \"%s\\n\" \"$$\" > \"$1.tmp\" && mv \"$1.tmp\" \"$1\" && exec /bin/sleep 30' agentctl-holder \"$2\" & wait",
                    "agentctl-test",
                ])
                .arg(&self.group_pid_file)
                .arg(&self.escaped_pid_file);
            command
        }

        fn group_stdout_overflow_command(&self) -> Command {
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    "/bin/sleep 30 & printf '%s\\n' \"$!\" > \"$1\"; exec /usr/bin/head -c \"$2\" /dev/zero",
                    "agentctl-test",
                ])
                .arg(&self.group_pid_file)
                .arg((CONTROL_STDOUT_BYTES + 1).to_string());
            command
        }

        fn read_pid(path: &Path) -> libc::pid_t {
            fs::read_to_string(path)
                .expect("fixture child wrote its pid")
                .trim()
                .parse()
                .expect("fixture child pid is numeric")
        }

        fn wait_for_pid(path: &Path, timeout: Duration) -> Option<libc::pid_t> {
            let deadline = Instant::now() + timeout;
            loop {
                if let Ok(value) = fs::read_to_string(path) {
                    return Some(value.trim().parse().expect("fixture child pid is numeric"));
                }
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn group_pid(&self) -> libc::pid_t {
            Self::read_pid(&self.group_pid_file)
        }

        fn escaped_pid(&self) -> libc::pid_t {
            Self::read_pid(&self.escaped_pid_file)
        }

        fn terminate_escaped(&self) {
            let pid = self.escaped_pid();
            // SAFETY: the fixture recorded this exact process immediately before this call.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
            assert!(
                wait_until_process_is_gone(pid, Duration::from_secs(2)),
                "escaped pipe holder must be cleaned after the assertion"
            );
            fs::remove_file(&self.escaped_pid_file).expect("retire escaped pid marker");
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for RecordedChild {
        fn drop(&mut self) {
            if let Ok(value) = fs::read_to_string(&self.escaped_pid_file) {
                if let Ok(pid) = value.trim().parse::<libc::pid_t>() {
                    // SAFETY: this fixture recorded the exact process it created and is solely
                    // responsible for bounding that deliberately escaped test descendant.
                    let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(target_os = "linux")]
    struct ChildGuard(Child);

    #[cfg(target_os = "linux")]
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(target_os = "linux")]
    struct ProcessGroupGuard(libc::pid_t);

    #[cfg(target_os = "linux")]
    impl Drop for ProcessGroupGuard {
        fn drop(&mut self) {
            // SAFETY: tests construct this guard only for a fresh process group they own.
            let _ = unsafe { libc::kill(-self.0, libc::SIGKILL) };
        }
    }

    #[cfg(target_os = "linux")]
    fn install_test_elf(source: &str, destination: &Path) {
        fs::copy(source, destination).unwrap();
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(target_os = "linux")]
    fn spawn_test_process(executable: &Path) -> ChildGuard {
        let mut command = Command::new(executable);
        command.arg("30").process_group(0);
        ChildGuard(command.spawn().expect("start test executable"))
    }

    #[cfg(target_os = "linux")]
    fn process_info_response(pid: u32, executable: &Path) -> String {
        serde_json::json!({
            "result": {
                "process_info": {
                    "pane_id": "pane",
                    "foreground_process_group_id": pid,
                    "foreground_processes": [{
                        "pid": pid,
                        "argv": [executable.display().to_string(), "30"]
                    }]
                }
            }
        })
        .to_string()
    }

    #[cfg(target_os = "linux")]
    extern "C" fn test_signal_handler(_signal: libc::c_int) {}

    #[cfg(target_os = "linux")]
    struct SignalHandlerGuard {
        previous_action: libc::sigaction,
        previous_mask: libc::sigset_t,
    }

    #[cfg(target_os = "linux")]
    impl SignalHandlerGuard {
        fn install() -> Self {
            // SAFETY: zero is a valid starting representation for sigaction/sigset_t before the
            // libc initialization calls below fill their public fields.
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            action.sa_sigaction = test_signal_handler as *const () as usize;
            // SAFETY: action owns writable mask storage.
            assert_eq!(unsafe { libc::sigemptyset(&mut action.sa_mask) }, 0);
            let mut previous_action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
            // SAFETY: both action pointers remain live and sigaction initializes the old action.
            assert_eq!(
                unsafe { libc::sigaction(libc::SIGUSR1, &action, previous_action.as_mut_ptr()) },
                0
            );
            // SAFETY: successful sigaction initialized previous_action.
            let previous_action = unsafe { previous_action.assume_init() };

            // Ensure the test thread can receive the targeted signal even if its harness parent
            // happened to block SIGUSR1, and restore its exact mask afterward.
            // SAFETY: zeroed sigset_t is initialized by sigemptyset before use.
            let mut signal_set = unsafe { std::mem::zeroed::<libc::sigset_t>() };
            // SAFETY: signal_set is writable and SIGUSR1 is valid.
            assert_eq!(unsafe { libc::sigemptyset(&mut signal_set) }, 0);
            assert_eq!(
                unsafe { libc::sigaddset(&mut signal_set, libc::SIGUSR1) },
                0
            );
            let mut previous_mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            // SAFETY: signal_set is initialized and previous_mask is writable.
            assert_eq!(
                unsafe {
                    libc::pthread_sigmask(
                        libc::SIG_UNBLOCK,
                        &signal_set,
                        previous_mask.as_mut_ptr(),
                    )
                },
                0
            );
            // SAFETY: successful pthread_sigmask initialized previous_mask.
            let previous_mask = unsafe { previous_mask.assume_init() };
            Self {
                previous_action,
                previous_mask,
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for SignalHandlerGuard {
        fn drop(&mut self) {
            // SAFETY: both saved values came from successful libc queries in install.
            assert_eq!(
                unsafe {
                    libc::pthread_sigmask(
                        libc::SIG_SETMASK,
                        &self.previous_mask,
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            // SAFETY: previous_action was initialized by sigaction for this same signal.
            assert_eq!(
                unsafe {
                    libc::sigaction(libc::SIGUSR1, &self.previous_action, std::ptr::null_mut())
                },
                0
            );
        }
    }

    #[cfg(target_os = "linux")]
    fn wait_until_process_is_gone(pid: libc::pid_t, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: signal zero only probes the numeric identity recorded by this test fixture.
            if unsafe { libc::kill(pid, 0) } < 0
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn allocation_arguments_preserve_literal_values() {
        let environment = [
            "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai".to_owned(),
            "LITERAL=a b=$(unexpanded)=tail".to_owned(),
        ];
        assert_eq!(
            tab_create_arguments("workspace", "worker", "/tmp", &environment),
            strings(&[
                "tab",
                "create",
                "--workspace",
                "workspace",
                "--label",
                "worker",
                "--cwd",
                "/tmp",
                "--env",
                "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai",
                "--env",
                "LITERAL=a b=$(unexpanded)=tail",
                "--no-focus",
            ])
        );
        assert_eq!(
            workspace_create_arguments("subagents", "/tmp", &environment),
            strings(&[
                "workspace",
                "create",
                "--label",
                "subagents",
                "--cwd",
                "/tmp",
                "--env",
                "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai",
                "--env",
                "LITERAL=a b=$(unexpanded)=tail",
                "--no-focus",
            ])
        );
    }

    #[test]
    fn pane_identity_parser_rejects_a_different_pane() {
        let document = serde_json::json!({
            "pane": {
                "pane_id": "other",
                "workspace_id": "workspace",
                "cwd": "/tmp",
                "agent": "codex",
                "agent_status": "idle"
            }
        });
        let result = document.as_object().expect("fixture object");
        let error = parse_pane_info(result, "expected").expect_err("mismatch must be refused");
        assert!(error.to_string().contains("other"));
        assert!(error.to_string().contains("expected"));
    }

    #[test]
    fn pane_info_parser_reads_scroll_geometry_and_refuses_a_malformed_one() {
        let parse = |scroll: Option<Value>| {
            let mut pane = serde_json::json!({
                "pane_id": "pane",
                "workspace_id": "workspace",
                "cwd": "/tmp",
                "agent": "claude",
                "agent_status": "idle"
            });
            if let Some(scroll) = scroll {
                pane["scroll"] = scroll;
            }
            let document = serde_json::json!({ "pane": pane });
            parse_pane_info(document.as_object().expect("fixture object"), "pane")
        };

        // Claude Code redraws one screen in place, so Herdr reports that it keeps no scrollback.
        let info = parse(Some(serde_json::json!({
            "offset_from_bottom": 0,
            "max_offset_from_bottom": 0,
            "viewport_rows": 52
        })))
        .expect("screen-only pane");
        assert_eq!(
            info.scroll,
            Some(PaneScroll {
                offset_from_bottom: 0,
                max_offset_from_bottom: 0,
                viewport_rows: 52,
            })
        );
        assert!(info.keeps_no_scrollback());

        let info = parse(Some(serde_json::json!({
            "offset_from_bottom": 3,
            "max_offset_from_bottom": 120,
            "viewport_rows": 40
        })))
        .expect("pane with scrollback");
        assert_eq!(
            info.scroll,
            Some(PaneScroll {
                offset_from_bottom: 3,
                max_offset_from_bottom: 120,
                viewport_rows: 40,
            })
        );
        assert!(!info.keeps_no_scrollback());

        // A Herdr that reports no geometry leaves it unknown, which is not the same as none.
        for scroll in [None, Some(Value::Null)] {
            let info = parse(scroll).expect("pane without geometry");
            assert_eq!(info.scroll, None);
            assert!(!info.keeps_no_scrollback());
        }

        for (scroll, named) in [
            (serde_json::json!(0), "'scroll' is not an object"),
            (
                serde_json::json!({ "offset_from_bottom": 0, "viewport_rows": 52 }),
                "\"max_offset_from_bottom\"",
            ),
            (
                serde_json::json!({
                    "offset_from_bottom": 0,
                    "max_offset_from_bottom": -1,
                    "viewport_rows": 52
                }),
                "\"max_offset_from_bottom\"",
            ),
            (
                serde_json::json!({
                    "offset_from_bottom": "0",
                    "max_offset_from_bottom": 0,
                    "viewport_rows": 52
                }),
                "\"offset_from_bottom\"",
            ),
            (
                serde_json::json!({
                    "offset_from_bottom": 0,
                    "max_offset_from_bottom": 0,
                    "viewport_rows": 52.5
                }),
                "\"viewport_rows\"",
            ),
        ] {
            let error = parse(Some(scroll.clone())).expect_err("malformed geometry is refused");
            assert!(
                error.to_string().contains(named),
                "{scroll}: {error} does not name {named}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancellation_and_timeout_preserve_primary_kind_with_cleanup_uncertainty() {
        for (cancelled, timeout, expected_kind, primary_text) in [
            (
                true,
                Duration::from_secs(30),
                io::ErrorKind::Interrupted,
                "control command was cancelled",
            ),
            (
                false,
                Duration::ZERO,
                io::ErrorKind::TimedOut,
                "control command timed out",
            ),
        ] {
            let mut shutdown = |_child: &mut ProcessPluginChild, _grace: Duration| {
                Err(io::Error::other("injected shutdown failure"))
            };
            let mut command = Command::new("/bin/sleep");
            command.arg("30");
            let error = bounded_output_with_cancellation_and_shutdown(
                command,
                timeout,
                &|| cancelled,
                &mut shutdown,
            )
            .expect_err("primary stop plus failed shutdown must be reported");
            assert_eq!(error.kind(), expected_kind);
            let message = error.to_string();
            assert!(message.contains(primary_text), "{message}");
            assert!(
                message.contains("process cleanup is uncertain"),
                "{message}"
            );
            assert!(message.contains("injected shutdown failure"), "{message}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn poll_interruptions_do_not_extend_the_absolute_deadline() {
        let _fixture = SIGNAL_FIXTURE.lock().expect("lock signal fixture");
        let _handler = SignalHandlerGuard::install();
        let target = unsafe { libc::pthread_self() };
        let stop = Arc::new(AtomicBool::new(false));
        let sender_stop = Arc::clone(&stop);
        let sender = thread::spawn(move || {
            while !sender_stop.load(Ordering::SeqCst) {
                // SAFETY: target names the live test thread until stop is set and this worker is
                // joined. SIGUSR1 has the no-op handler installed above.
                assert_eq!(unsafe { libc::pthread_kill(target, libc::SIGUSR1) }, 0);
                thread::sleep(Duration::from_millis(1));
            }
        });
        let interrupts_before = POLL_CAPTURE_INTERRUPTS.load(Ordering::SeqCst);
        let started = Instant::now();
        let deadline = started + Duration::from_millis(150);
        while Instant::now() < deadline {
            poll_capture(
                None,
                None,
                deadline.saturating_duration_since(Instant::now()),
            )
            .expect("EINTR returns control to the absolute-deadline loop");
        }
        let elapsed = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        sender.join().expect("join signal sender");
        assert!(elapsed >= Duration::from_millis(100), "elapsed={elapsed:?}");
        assert!(elapsed < Duration::from_secs(1), "elapsed={elapsed:?}");
        assert!(
            POLL_CAPTURE_INTERRUPTS.load(Ordering::SeqCst) > interrupts_before,
            "fixture must causally exercise poll's EINTR branch"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn bounded_capture_refuses_before_executing_on_unsupported_platforms() {
        let error = bounded_output(Command::new("must-not-run"), Duration::from_secs(1))
            .expect_err("non-Linux control capture must be refused");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tab_allocation_captures_root_pane_without_a_followup_query() {
        let executable = FakeExecutable::new(
            r#"{"result":{"tab":{"tab_id":"tab"},"root_pane":{"pane_id":"pane","tab_id":"tab","workspace_id":"workspace"}}}"#,
        );
        assert_eq!(
            executable
                .client()
                .create_tab_with_pane(
                    "workspace",
                    "worker",
                    "/tmp",
                    &[
                        "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai".to_owned(),
                        "LITERAL=a b=$(unexpanded)=tail".to_owned(),
                    ],
                )
                .unwrap(),
            ("tab".to_owned(), "pane".to_owned())
        );
        assert_eq!(
            executable.arguments(),
            serde_json::json!([
                "tab",
                "create",
                "--workspace",
                "workspace",
                "--label",
                "worker",
                "--cwd",
                "/tmp",
                "--env",
                "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai",
                "--env",
                "LITERAL=a b=$(unexpanded)=tail",
                "--no-focus"
            ])
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pane_move_captures_the_reassigned_identity_and_exact_destination() {
        let executable = FakeExecutable::new(
            r#"{"result":{"move_result":{"changed":true,"previous_pane_id":"w1:p1","pane":{"pane_id":"w2:p2","tab_id":"w2:t2","workspace_id":"w2"}}}}"#,
        );
        assert_eq!(
            executable
                .client()
                .move_pane_to_new_tab("w1:p1", "w2", "worker")
                .unwrap(),
            Pane {
                pane_id: "w2:p2".to_owned(),
                tab_id: "w2:t2".to_owned(),
                workspace_id: "w2".to_owned(),
            }
        );
        assert_eq!(
            executable.arguments(),
            serde_json::json!([
                "pane",
                "move",
                "w1:p1",
                "--new-tab",
                "--workspace",
                "w2",
                "--label",
                "worker",
                "--no-focus"
            ])
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pane_move_refuses_a_response_for_a_different_destination() {
        let executable = FakeExecutable::new(
            r#"{"result":{"move_result":{"changed":true,"previous_pane_id":"w1:p1","pane":{"pane_id":"w3:p2","tab_id":"w3:t2","workspace_id":"w3"}}}}"#,
        );
        let error = executable
            .client()
            .move_pane_to_new_tab("w1:p1", "w2", "worker")
            .unwrap_err();
        assert!(error.to_string().contains("expected \"w2\""), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pane_move_refuses_incomplete_transition_proof() {
        for (changed, previous, message) in [
            (false, "w1:p1", "no move occurred"),
            (true, "wrong", "previous pane identity"),
        ] {
            let executable = FakeExecutable::new(&format!(
                r#"{{"result":{{"move_result":{{"changed":{changed},"previous_pane_id":"{previous}","pane":{{"pane_id":"w2:p2","tab_id":"w2:t2","workspace_id":"w2"}}}}}}}}"#,
            ));
            let error = executable
                .client()
                .move_pane_to_new_tab("w1:p1", "w2", "worker")
                .unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn workspace_allocation_passes_literal_environment() {
        let executable = FakeExecutable::new(
            r#"{"result":{"workspace":{"workspace_id":"workspace"},"tab":{"tab_id":"tab"},"root_pane":{"pane_id":"pane"}}}"#,
        );
        assert_eq!(
            executable
                .client()
                .create_workspace(
                    "subagents",
                    "/tmp",
                    &[
                        "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai".to_owned(),
                        "LITERAL=a b=$(unexpanded)=tail".to_owned(),
                    ],
                )
                .unwrap(),
            ("workspace".to_owned(), "tab".to_owned(), "pane".to_owned())
        );
        assert_eq!(
            executable.arguments(),
            serde_json::json!([
                "workspace",
                "create",
                "--label",
                "subagents",
                "--cwd",
                "/tmp",
                "--env",
                "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai",
                "--env",
                "LITERAL=a b=$(unexpanded)=tail",
                "--no-focus"
            ])
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_capture_preserves_stdout_stderr_and_exit_status() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf 'ordinary stdout'; printf 'ordinary stderr' >&2; exit 7",
        ]);
        let output = bounded_output(command, Duration::from_secs(5)).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"ordinary stdout");
        assert_eq!(output.stderr, b"ordinary stderr");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn focus_uses_agent_focus_and_shutdown_targets_one_exact_pane() {
        let executable = FakeExecutable::new("{}");
        executable.client().focus_pane("workspace:pane").unwrap();
        assert_eq!(
            executable.arguments(),
            serde_json::json!(["agent", "focus", "workspace:pane"])
        );
        executable.client().close_pane("workspace:pane").unwrap();
        assert_eq!(
            executable.arguments(),
            serde_json::json!(["pane", "close", "workspace:pane"])
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recorded_pane_shell_identity_refuses_every_generation_change() {
        let executable = fs::canonicalize("/bin/bash").unwrap();
        let mut command = Command::new(&executable);
        command
            .args(["--noprofile", "--norc"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let shell = ChildGuard(command.spawn().expect("start real test shell"));
        let pid = shell.0.id();
        let response = |argv0: &Path| {
            serde_json::json!({
                "result": {
                    "process_info": {
                        "pane_id": "pane",
                        "shell_pid": pid,
                        "foreground_process_group_id": pid,
                        "foreground_processes": [{
                            "pid": pid,
                            "argv": [argv0.display().to_string()]
                        }]
                    }
                }
            })
            .to_string()
        };
        let herdr = FakeExecutable::new(&response(&executable));
        let identity = herdr.client().pane_shell_identity("pane").unwrap();
        assert!(herdr
            .client()
            .pane_is_same_idle_shell("pane", &identity)
            .unwrap());

        let mut mismatches = Vec::new();
        let mut value = identity.clone();
        value.boot_id = "ffffffff-ffff-ffff-ffff-ffffffffffff".to_owned();
        mismatches.push(value);
        let mut value = identity.clone();
        value.pid += 1;
        mismatches.push(value);
        let mut value = identity.clone();
        value.starttime_ticks += 1;
        mismatches.push(value);
        let mut value = identity.clone();
        value.executable_device += 1;
        mismatches.push(value);
        let mut value = identity.clone();
        value.executable_inode += 1;
        mismatches.push(value);
        for mismatch in mismatches {
            assert!(!herdr
                .client()
                .pane_is_same_idle_shell("pane", &mismatch)
                .unwrap());
        }

        herdr.set_response(&response(Path::new("/usr/bin/sleep")));
        assert!(!herdr
            .client()
            .pane_is_same_idle_shell("pane", &identity)
            .unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_non_shell_process_cannot_be_adopted_as_a_pane_shell() {
        let executable = fs::canonicalize("/usr/bin/sleep").unwrap();
        let process = spawn_test_process(&executable);
        let pid = process.0.id();
        let response = serde_json::json!({
            "result": {
                "process_info": {
                    "pane_id": "pane",
                    "shell_pid": pid,
                    "foreground_process_group_id": pid,
                    "foreground_processes": [{
                        "pid": pid,
                        "argv": [executable.display().to_string(), "30"]
                    }]
                }
            }
        })
        .to_string();
        let herdr = FakeExecutable::new(&response);
        let identity = live_custom_process(u64::from(pid)).unwrap().identity;

        let error = herdr.client().pane_shell_identity("pane").unwrap_err();
        assert!(error.to_string().contains("supported identity-bound shell"));
        assert!(!herdr.client().pane_is_idle_shell("pane").unwrap());
        assert!(!herdr
            .client()
            .pane_is_same_idle_shell("pane", &identity)
            .unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_supported_shell_with_background_descendant_is_not_idle() {
        let executable = fs::canonicalize("/bin/bash").unwrap();
        let mut command = Command::new(&executable);
        command
            .args(["--noprofile", "--norc"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let mut shell = ChildGuard(command.spawn().expect("start real test shell"));
        let pid = shell.0.id();
        let _group = ProcessGroupGuard(pid as libc::pid_t);
        shell
            .0
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"/usr/bin/sleep 30 & wait\n")
            .unwrap();
        shell.0.stdin.as_mut().unwrap().flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && process_has_no_descendants(u64::from(pid)).unwrap() {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(shell.0.try_wait().unwrap().is_none());
        assert!(!process_has_no_descendants(u64::from(pid)).unwrap());
        let response = serde_json::json!({
            "result": {
                "process_info": {
                    "pane_id": "pane",
                    "shell_pid": pid,
                    "foreground_process_group_id": pid,
                    "foreground_processes": [{
                        "pid": pid,
                        "argv": [executable.display().to_string()]
                    }]
                }
            }
        })
        .to_string();
        let herdr = FakeExecutable::new(&response);
        let identity = herdr.client().pane_shell_identity("pane").unwrap();
        assert!(!herdr.client().pane_is_idle_shell("pane").unwrap());
        assert!(!herdr
            .client()
            .pane_is_same_idle_shell("pane", &identity)
            .unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recorded_custom_identity_survives_atomic_pathname_replacement() {
        let herdr = FakeExecutable::new("{}");
        let harness = herdr.root.join("muse");
        install_test_elf("/bin/sleep", &harness);
        let pinned = pin_harness_executable(harness.clone()).unwrap();
        let process = spawn_test_process(&harness);
        let pid = process.0.id();
        herdr.set_response(&process_info_response(pid, &harness));
        let identity = herdr
            .client()
            .custom_harness_observed("pane", &pinned, &|| false)
            .unwrap()
            .expect("observe the pinned launch image");

        let replacement = herdr.root.join("muse.replacement");
        install_test_elf("/bin/true", &replacement);
        fs::rename(&replacement, &harness).unwrap();

        herdr
            .client()
            .verify_custom_harness("pane", "muse", Some(&identity))
            .expect("verification uses the recorded image, not the replaced pathname");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recorded_custom_identity_refuses_replacement_process_and_field_mismatches() {
        let herdr = FakeExecutable::new("{}");
        let harness = herdr.root.join("muse");
        install_test_elf("/bin/sleep", &harness);
        let pinned = pin_harness_executable(harness.clone()).unwrap();
        let mut original = spawn_test_process(&harness);
        let original_pid = original.0.id();
        herdr.set_response(&process_info_response(original_pid, &harness));
        let identity = herdr
            .client()
            .custom_harness_observed("pane", &pinned, &|| false)
            .unwrap()
            .expect("observe original process");

        let mut mismatches = Vec::new();
        let mut value = identity.clone();
        value.boot_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned();
        mismatches.push(value);
        let mut value = identity.clone();
        value.pid = value.pid.saturating_add(1);
        mismatches.push(value);
        let mut value = identity.clone();
        value.starttime_ticks += 1;
        mismatches.push(value);
        let mut value = identity.clone();
        value.executable_device += 1;
        mismatches.push(value);
        let mut value = identity.clone();
        value.executable_inode += 1;
        mismatches.push(value);
        for mismatch in mismatches {
            assert!(herdr
                .client()
                .verify_custom_harness("pane", "muse", Some(&mismatch))
                .is_err());
        }
        let wrong_group = serde_json::json!({
            "result": {
                "process_info": {
                    "pane_id": "pane",
                    "foreground_process_group_id": u64::from(original_pid) + 1,
                    "foreground_processes": [{
                        "pid": original_pid,
                        "argv": [harness.display().to_string(), "30"]
                    }]
                }
            }
        })
        .to_string();
        herdr.set_response(&wrong_group);
        assert!(herdr
            .client()
            .verify_custom_harness("pane", "muse", Some(&identity))
            .is_err());
        let duplicate = serde_json::json!({
            "result": {
                "process_info": {
                    "pane_id": "pane",
                    "foreground_process_group_id": original_pid,
                    "foreground_processes": [
                        {"pid": original_pid, "argv": [harness.display().to_string(), "30"]},
                        {"pid": u64::from(original_pid) + 1, "argv": [harness.display().to_string(), "30"]}
                    ]
                }
            }
        })
        .to_string();
        herdr.set_response(&duplicate);
        assert!(herdr
            .client()
            .custom_harness_observed("pane", &pinned, &|| false)
            .is_err());

        let duplicate_identity = serde_json::json!({
            "result": {
                "process_info": {
                    "pane_id": "pane",
                    "foreground_process_group_id": original_pid,
                    "foreground_processes": [
                        {"pid": original_pid, "argv": [harness.display().to_string(), "30"]},
                        {"pid": original_pid, "argv": [harness.display().to_string(), "30"]}
                    ]
                }
            }
        })
        .to_string();
        herdr.set_response(&duplicate_identity);
        assert!(herdr
            .client()
            .verify_custom_harness("pane", "muse", Some(&identity))
            .is_err());

        original.0.kill().unwrap();
        original.0.wait().unwrap();
        let replacement = herdr.root.join("muse.replacement");
        install_test_elf("/bin/sleep", &replacement);
        fs::rename(&replacement, &harness).unwrap();
        let replacement = spawn_test_process(&harness);
        herdr.set_response(&process_info_response(replacement.0.id(), &harness));
        assert!(herdr
            .client()
            .verify_custom_harness("pane", "muse", Some(&identity))
            .is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn custom_harness_pin_refuses_interpreter_payloads() {
        let herdr = FakeExecutable::new("{}");
        let harness = herdr.root.join("muse-script");
        fs::write(&harness, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&harness, fs::Permissions::from_mode(0o700)).unwrap();
        let error = pin_harness_executable(harness).unwrap_err();
        assert!(error.to_string().contains("ELF image"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mismatched_pane_identity_is_rejected() {
        let executable = FakeExecutable::new(
            r#"{"result":{"pane":{"pane_id":"other","workspace_id":"workspace","cwd":"/tmp","agent":"codex","agent_status":"idle"}}}"#,
        );
        assert!(executable
            .client()
            .pane_info("expected")
            .unwrap_err()
            .to_string()
            .contains("expected"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn control_timeout_includes_pipes_inherited_after_the_parent_exits() {
        let mut unrelated = ChildGuard(Command::new("/bin/sleep").arg("30").spawn().unwrap());
        // The timeout remains explicit, but its clock starts at spawn, so a loaded host can fire
        // it before the shell forks the escaped holder. Such an attempt never exercised inherited
        // pipes; retry it with a longer budget. The separate EINTR test exercises the short
        // 150ms deadline.
        for timeout in [1, 3, 10].map(Duration::from_secs) {
            let escaped = RecordedChild::new("setsid-holder");
            let started = Instant::now();
            let result = bounded_output(escaped.escaped_pipe_command(), timeout);
            assert_eq!(result.err().unwrap().kind(), io::ErrorKind::TimedOut);
            assert!(started.elapsed() < timeout + Duration::from_secs(4));
            // Once forked, the holder is outside the killed group and publishes itself.
            let Some(pid) =
                RecordedChild::wait_for_pid(&escaped.escaped_pid_file, Duration::from_secs(5))
            else {
                continue;
            };
            // The setsid child escaped the supervised group and really was retaining both capture
            // pipes, so returning promptly did not depend on receiving EOF from reader threads.
            assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
            // The pinned private group must not be confused with any unrelated numeric identity.
            assert!(unrelated.0.try_wait().unwrap().is_none());
            escaped.terminate_escaped();
            return;
        }
        panic!("the escaped pipe holder was never started before any timeout");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn control_cancellation_kills_command_group_and_inherited_pipes_promptly() {
        let escaped = RecordedChild::new("setsid-holder");
        let cancelled = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&cancelled);
        let group_pid_file = escaped.group_pid_file.clone();
        let escaped_pid_file = escaped.escaped_pid_file.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let markers_are_complete =
                    [&group_pid_file, &escaped_pid_file].iter().all(|path| {
                        fs::read_to_string(path)
                            .ok()
                            .and_then(|value| value.trim().parse::<libc::pid_t>().ok())
                            .is_some()
                    });
                if markers_are_complete || Instant::now() >= deadline {
                    trigger.store(true, Ordering::SeqCst);
                    return markers_are_complete;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let started = Instant::now();
        let result = bounded_output_with_cancellation(
            escaped.mixed_descendant_command(),
            Duration::from_secs(30),
            &|| cancelled.load(Ordering::SeqCst),
        );
        assert!(
            worker.join().expect("join cancellation trigger"),
            "cancellation fixture must record the escaped pipe holder before cancellation"
        );
        assert_eq!(
            result.expect_err("cancel command").kind(),
            io::ErrorKind::Interrupted
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            wait_until_process_is_gone(escaped.group_pid(), Duration::from_secs(2)),
            "cancellation must kill a live descendant in the supervised group"
        );
        let escaped_pid = escaped.escaped_pid();
        assert_eq!(unsafe { libc::kill(escaped_pid, 0) }, 0);
        assert_eq!(unsafe { libc::getsid(escaped_pid) }, escaped_pid);
        assert_eq!(unsafe { libc::getpgid(escaped_pid) }, escaped_pid);
        escaped.terminate_escaped();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn control_capture_enforces_independent_stream_byte_caps() {
        let group_child = RecordedChild::new("group-holder");
        let stdout_error = bounded_output(
            group_child.group_stdout_overflow_command(),
            Duration::from_secs(5),
        )
        .expect_err("oversized stdout must be refused");
        assert_eq!(stdout_error.kind(), io::ErrorKind::InvalidData);
        assert!(stdout_error.to_string().contains("stdout"));
        assert!(
            wait_until_process_is_gone(group_child.group_pid(), Duration::from_secs(2)),
            "output-cap refusal must terminate a descendant in the supervised group"
        );

        let mut stderr_command = Command::new("/bin/sh");
        stderr_command
            .args([
                "-c",
                "exec /usr/bin/head -c \"$1\" /dev/zero >&2",
                "agentctl-test",
            ])
            .arg((CONTROL_STDERR_BYTES + 1).to_string());
        let stderr_error = bounded_output(stderr_command, Duration::from_secs(5))
            .expect_err("oversized stderr must be refused");
        assert_eq!(stderr_error.kind(), io::ErrorKind::InvalidData);
        assert!(stderr_error.to_string().contains("stderr"));
    }

    #[test]
    fn input_expect_capability_is_read_from_the_capabilities_line() {
        assert!(advertises_input_expect(
            "server: running\ncapabilities: events, input-expect\n"
        ));
        assert!(advertises_input_expect("capabilities:input-expect"));
        assert!(!advertises_input_expect("capabilities: events\n"));
        assert!(!advertises_input_expect("input-expect: yes\n"));
        assert!(!advertises_input_expect(
            "capabilities: input-expectation\n"
        ));
    }

    #[test]
    fn expectation_failed_is_recognised_with_or_without_spaces() {
        assert!(expectation_failed(
            r#"{"error": {"code": "expectation_failed", "message": "terminal changed"}}"#
        ));
        assert!(expectation_failed(
            r#"{"error":{"code":"expectation_failed"}}"#
        ));
        assert!(!expectation_failed(
            r#"{"error":{"code":"pane_not_found"}}"#
        ));
        assert!(!expectation_failed("expectation_failed"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn expected_terminal_precedes_positionals_and_a_refusal_is_typed() {
        let executable = FakeExecutable::new("{}");
        let script = "#!/usr/bin/python3\nimport json, pathlib, sys\npathlib.Path(__file__).with_name('args').write_text(json.dumps(sys.argv[1:]))\nsys.stderr.write('{\"error\": {\"code\": \"expectation_failed\", \"message\": \"terminal changed\"}}\\n')\nsys.exit(1)\n";
        fs::write(executable.root.join("herdr"), script).unwrap();
        fs::set_permissions(
            executable.root.join("herdr"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let client = executable.client();
        let error = client
            .send_text_expecting("w1:p1", "hello", Some("term-1"), &|| false)
            .unwrap_err();
        assert_eq!(
            error.kind(),
            crate::error::AdapterErrorKind::ExpectationFailed
        );
        assert_eq!(
            executable.arguments(),
            serde_json::json!([
                "pane",
                "send-text",
                "--expect-terminal",
                "term-1",
                "w1:p1",
                "hello"
            ])
        );
        let error = client
            .agent_prompt("w1:p1", "hi", Some("term-1"), &|| false)
            .unwrap_err();
        assert_eq!(
            error.kind(),
            crate::error::AdapterErrorKind::ExpectationFailed
        );
        assert_eq!(
            executable.arguments(),
            serde_json::json!([
                "agent",
                "prompt",
                "--expect-terminal",
                "term-1",
                "w1:p1",
                "hi"
            ])
        );
        // Without an expectation the same refusal is an ordinary unknown outcome.
        let error = client
            .send_keys_expecting("w1:p1", "Enter", None, &|| false)
            .unwrap_err();
        assert_eq!(error.kind(), crate::error::AdapterErrorKind::Unavailable);
        assert_eq!(
            executable.arguments(),
            serde_json::json!(["pane", "send-keys", "w1:p1", "Enter"])
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn input_expect_support_is_read_once_per_client() {
        let executable = FakeExecutable::new("capabilities: input-expect");
        let client = executable.client();
        assert!(client.input_expect_supported().unwrap());
        assert_eq!(
            executable.arguments(),
            serde_json::json!(["status", "server"])
        );
        executable.set_response("capabilities: events");
        assert!(client.input_expect_supported().unwrap(), "cached");
        assert!(!executable.client().input_expect_supported().unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_queries_parse_herdr_responses() {
        let executable = FakeExecutable::new(
            r#"{"result":{"agent":{"name":"worker","pane_id":"w1:p1","tab_id":"w1:t1","terminal_id":"term-9"}}}"#,
        );
        assert_eq!(
            executable.client().agent_identity("worker").unwrap(),
            AgentIdentity {
                name: "worker".to_owned(),
                pane_id: "w1:p1".to_owned(),
                tab_id: Some("w1:t1".to_owned()),
                terminal_id: Some("term-9".to_owned()),
            }
        );
        assert!(executable.client().agent_identity("other").is_err());
        executable.set_response(
            r#"{"result":{"agents":[{"name":"worker","pane_id":"w1:p1"},{"name":null,"pane_id":"w1:p2"}]}}"#,
        );
        let names = executable.client().agent_names().unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names["worker"], "w1:p1");
        executable.set_response(r#"{"result":{"tabs":[{"tab_id":"w1:t1","label":"worker"}]}}"#);
        assert_eq!(
            executable.client().tab_labels("w1").unwrap()["w1:t1"],
            "worker"
        );
        assert_eq!(
            executable.arguments(),
            serde_json::json!(["tab", "list", "--workspace", "w1"])
        );
        executable.set_response(r#"{"result":{"tab":{"tab_id":"w1:t1","label":"worker"}}}"#);
        assert_eq!(executable.client().tab_label("w1:t1").unwrap(), "worker");
        assert!(executable.client().tab_label("w1:t2").is_err());
        executable.set_response(
            r#"{"result":{"pane":{"pane_id":"w1:p1","workspace_id":"w1","cwd":"/","terminal_id":"term-9","tab_id":"w1:t1"}}}"#,
        );
        let info = executable.client().pane_info("w1:p1").unwrap();
        assert_eq!(info.terminal_id.as_deref(), Some("term-9"));
        assert_eq!(info.tab_id.as_deref(), Some("w1:t1"));
        executable.set_response(r#"{"result":{}}"#);
        executable
            .client()
            .rename_agent("w1:p1", "reviewer")
            .unwrap();
        assert_eq!(
            executable.arguments(),
            serde_json::json!(["agent", "rename", "w1:p1", "reviewer"])
        );
    }
}
