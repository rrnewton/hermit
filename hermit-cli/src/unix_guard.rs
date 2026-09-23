//! Outside exec-created Unix policy owner and explicit Container fork split.
//!
//! Source candidate: ordinary mode selection does not call this entry yet.
//! No parent Arc, service thread, Tokio object or libbpf session crosses clone.
use std::cell::Cell;
use std::ffi::CString;
use std::io;
use std::marker::PhantomData;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::time::Duration;
use std::time::Instant;

use detcore::network_runtime::capability_unit::CAPABILITY_ENVIRONMENT;
use detcore::network_runtime::capability_unit::CAPABILITY_SUDO;
use detcore::network_runtime::capability_unit::CapabilityServiceKind;
use detcore::network_runtime::capability_unit::CapabilityUnitLaunch;
use reverie::process::Container;
use reverie::process::OwnedDeferredContainerRun;
use reverie::process::StartupError;
use reverie::process::StartupOwnedFailure;

const MAGIC: u64 = 0x5547_4b45_4550_3031;
const INIT: u32 = 1;
const ARM: u32 = 2;
const BIRTH: u32 = 3;
const INITIAL: u32 = 4;
const CREATOR_RECOVERY: u32 = 6;
const CONTROLLER_CHANNEL: u32 = 7;
const CONTROLLER_TASK: u32 = 8;
const TERMINAL: u32 = 9;
const PIDFD_THREAD: libc::c_uint = 1;
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Frame {
    magic: u64,
    incarnation: u64,
    sequence: u64,
    operation: u32,
    rights: u32,
    error: i32,
    reserved: u32,
    values: [u64; 8],
}
#[repr(C)]
struct Packet {
    frame: Frame,
    fds: [i32; 4],
    count: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct GuardDenial {
    pub phase: u64,
    pub incarnation: u64,
    pub task_start: u64,
    pub pid_tgid: u64,
    pub source_generation: u64,
    pub peer_generation: u64,
    pub injection: u64,
    pub hook: u32,
    pub reason: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct GuardEvidence {
    primary: i32,
    pub secondary_monitor_failures: u64,
    pub secondary_guard_faults: u64,
    pub denial: GuardDenial,
}
#[repr(C)]
struct MonitorFds {
    config: i32,
    status: i32,
    ring: i32,
    other_actor_pidfd: i32,
    incarnation: u64,
}
/// Native keeper exit codes never select this enum. Only the validated sticky
/// map reader can produce a policy primary; helper/cleanup failures stay context.
#[derive(Clone, Copy, Debug)]
pub enum GuardOutcome {
    Pending,
    Running,
    Policy(GuardEvidence),
    Internal(GuardEvidence),
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct CreatorMask {
    original: u64,
    tid: i32,
    active: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct NativeBirth {
    incarnation: u64,
    sequence: u64,
    phase: u64,
    in_copy: u64,
    object: u64,
    generation: u64,
    cookie: u64,
}
unsafe extern "C" {
    fn ug_monitor_snapshot(fds: *const MonitorFds, evidence: *mut GuardEvidence) -> i32;
    fn ug_creator_mask_block(mask: *mut CreatorMask) -> i32;
    fn ug_creator_mask_restore(mask: *mut CreatorMask, child_branch: i32) -> i32;
    fn ug_creator_terminalize(
        map: i32,
        creator: i32,
        helper: i32,
        incarnation: u64,
        sequence: u64,
        acknowledged: i32,
        observed: *mut NativeBirth,
    ) -> i32;
    fn ug_channel_request_observed(
        fd: i32,
        request: *const Packet,
        response: *mut Packet,
        deadline: u64,
        verified_response: *mut i32,
    ) -> i32;
    fn ug_monitor_once(fds: *const MonitorFds, evidence: *mut GuardEvidence) -> i32;
}
const _: () = {
    assert!(std::mem::size_of::<Frame>() == 104);
    assert!(std::mem::size_of::<Packet>() == 128);
    assert!(std::mem::size_of::<GuardDenial>() == 64);
    assert!(std::mem::size_of::<GuardEvidence>() == 88);
    assert!(std::mem::size_of::<MonitorFds>() == 24);
    assert!(std::mem::size_of::<CreatorMask>() == 16);
    assert!(std::mem::size_of::<NativeBirth>() == 56);
};
fn observe(fds: MonitorFds, evidence: &mut GuardEvidence) -> GuardOutcome {
    let returned = unsafe { ug_monitor_once(&fds, evidence) };
    match evidence.primary {
        1 => GuardOutcome::Policy(*evidence),
        2 => GuardOutcome::Internal(*evidence),
        0 if returned > 0 => GuardOutcome::Pending,
        0 if returned == 0 => GuardOutcome::Running,
        _ => {
            evidence.primary = 2;
            GuardOutcome::Internal(*evidence)
        }
    }
}

/// Kernel-issued namespace birth, retained as a complete value. A pointer is
/// never an identity without its nonzero generation/cookie and this exact run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuardBirth {
    pub incarnation: u64,
    pub sequence: u64,
    pub object: u64,
    pub generation: u64,
    pub cookie: u64,
}
impl GuardBirth {
    fn decode(values: [u64; 8], incarnation: u64, sequence: u64) -> io::Result<Self> {
        const COMMITTED: u64 = 5; // ug_birth_phase, unix-guard.h
        if incarnation == 0
            || sequence == 0
            || values[0] != incarnation
            || values[1] != sequence
            || values[2] != COMMITTED
            || values[3] != 0
            || values[4] == 0
            || values[5] == 0
            || values[6] == 0
            || values[7] != 0
        {
            return Err(io::Error::other("guard birth receipt identity/phase"));
        }
        Ok(Self {
            incarnation,
            sequence,
            object: values[4],
            generation: values[5],
            cookie: values[6],
        })
    }
}

struct Reply {
    frame: Frame,
    rights: Vec<OwnedFd>,
}
fn raw_request(
    channel: BorrowedFd<'_>,
    incarnation: u64,
    sequence: u64,
    operation: u32,
    value: u64,
    rights: &[BorrowedFd<'_>],
    deadline: Instant,
) -> Result<Reply, RequestFailure> {
    let mut request = Packet {
        frame: Frame {
            magic: MAGIC,
            incarnation,
            sequence,
            operation,
            rights: rights.len() as u32,
            ..Frame::default()
        },
        fds: [-1; 4],
        count: rights.len() as u32,
    };
    if rights.len() > 4 {
        return Err(RequestFailure {
            error: io::Error::other("guard rights bound"),
            rights: Vec::new(),
            verified_remote_error: false,
        });
    }
    for (slot, right) in request.fds.iter_mut().zip(rights) {
        *slot = right.as_raw_fd();
    }
    request.frame.values[0] = value;
    let mut response = Packet {
        frame: Frame::default(),
        fds: [-1; 4],
        count: 0,
    };
    let nanos = match monotonic_deadline(deadline) {
        Ok(nanos) => nanos,
        Err(error) => {
            return Err(RequestFailure {
                error,
                rights: Vec::new(),
                verified_remote_error: false,
            });
        }
    };
    let mut verified_response = 0;
    let result = unsafe {
        ug_channel_request_observed(
            channel.as_raw_fd(),
            &request,
            &mut response,
            nanos,
            &mut verified_response,
        )
    };
    let error = (result != 0).then(io::Error::last_os_error);
    // Even malformed/failed receives have delivered real CLOEXEC descriptions.
    // They become owned immediately and accompany the primary failure.
    let received = response
        .fds
        .into_iter()
        .take(response.count.min(4) as usize)
        .filter(|fd| *fd >= 0)
        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect();
    let verified_remote_error = verified_response == 1
        && response.frame.magic == MAGIC
        && response.frame.incarnation == incarnation
        && response.frame.sequence == sequence
        && response.frame.operation == (operation | 0x100)
        && response.frame.reserved == 0
        && response.frame.error > 0
        && response.frame.rights == response.count
        && response.count <= 4;
    if let Some(error) = error {
        Err(RequestFailure {
            error,
            rights: received,
            verified_remote_error,
        })
    } else {
        Ok(Reply {
            frame: response.frame,
            rights: received,
        })
    }
}
fn duplicate_above_stdio(fd: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if raw < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }
}
fn monotonic_deadline(deadline: Instant) -> io::Result<u64> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "guard startup deadline"))?;
    let mut now: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
        return Err(io::Error::last_os_error());
    }
    (now.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|ns| ns.checked_add(now.tv_nsec as u64))
        .and_then(|ns| ns.checked_add(u64::try_from(remaining.as_nanos()).ok()?))
        .ok_or_else(|| io::Error::other("guard monotonic deadline overflow"))
}
#[must_use]
pub struct RequestFailure {
    pub error: io::Error,
    rights: Vec<OwnedFd>,
    verified_remote_error: bool,
}

/// Actual launcher-child wait ownership, separate from the helper pidfd and
/// policy cleanup. The PID remains protected
/// by exclusive non-auto-reaping parent ownership until waitpid; signals use
/// only the held pidfd. There is no implicit detach or numeric-PID kill in Drop.
struct LauncherChild {
    pid: libc::pid_t,
    pidfd: Option<OwnedFd>,
    status: Option<i32>,
}
impl LauncherChild {
    fn poll_wait(&mut self) -> io::Result<Option<i32>> {
        if self.status.is_some() {
            return Ok(self.status);
        }
        let mut status = 0;
        let returned = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if returned < 0 {
            return Err(io::Error::last_os_error());
        }
        if returned == self.pid {
            self.status = Some(status);
        } else if returned != 0 {
            return Err(io::Error::other("wrong owned keeper wait result"));
        }
        Ok(self.status)
    }
}

/// Holds the exact process, channels and partial returned rights on every error.
/// Policy pins and the durable external record additionally survive this owner
/// being lost. Neither process wait nor ordinary FD Drop is policy terminal proof.
struct OutsideGuardResources {
    elf: OwnedFd,
    bpffs_root: OwnedFd,
    recovery_root: OwnedFd,
}
#[must_use]
pub struct ParentGuard {
    unit: String,
    outside: Option<OutsideGuardResources>,
    launcher: LauncherChild,
    keeper_task: Option<OwnedFd>,
    channel: Option<OwnedFd>,
    controller_channel: Option<OwnedFd>,
    terminal: Option<GuardTerminalReceipt>,
    terminal_sequence: Option<u64>,
    terminal_in_flight: bool,
    terminal_proof: Option<([u64; 8], u64)>,
    last_request_verified: bool,
    parent_pidfd: OwnedFd,
    readers: Vec<OwnedFd>,
    unresolved_rights: Vec<OwnedFd>,
    controller: Option<OwnedFd>,
    incarnation: u64,
    next_sequence: u64,
    armed_sequence: Option<u64>,
    birth: Option<GuardBirth>,
    creator_tid: i32,
    creator_map: Option<OwnedFd>,
    creator_mask: CreatorMask,
    arm_acknowledged: bool,
    creator_terminal: Option<i32>,
    creator_recovery_error: Option<io::Error>,
    first_startup_error: Option<io::Error>,
    _same_thread: PhantomData<*mut ()>,
    monitor: GuardEvidence,
}
impl ParentGuard {
    /// Exact reviewed unit identity for the bounded shared stop/recovery path.
    /// The launcher process is never substituted for the actual keeper pidfd.
    pub fn unit(&self) -> &str {
        &self.unit
    }
    fn request(
        &mut self,
        op: u32,
        value: u64,
        rights: &[BorrowedFd<'_>],
        deadline: Instant,
    ) -> io::Result<Reply> {
        let sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("guard sequence exhausted"))?;
        self.next_sequence = sequence; // a failed/lost response never reuses it
        use std::os::fd::AsFd;
        let channel = self
            .channel
            .as_ref()
            .ok_or_else(|| io::Error::other("guard channel handed off"))?;
        self.last_request_verified = false;
        match raw_request(
            channel.as_fd(),
            self.incarnation,
            sequence,
            op,
            value,
            rights,
            deadline,
        ) {
            Ok(reply) => {
                self.last_request_verified = true;
                Ok(reply)
            }
            Err(mut failure) => {
                self.last_request_verified = failure.verified_remote_error;
                // Only a valid, matching INIT error frame gives the first
                // right its self-pidfd meaning. Truncated/malformed responses
                // remain unclassified recovery owners.
                if op == INIT && failure.verified_remote_error && failure.rights.len() == 1 {
                    self.keeper_task = Some(failure.rights.remove(0));
                }
                self.unresolved_rights.extend(failure.rights);
                Err(failure.error)
            }
        }
    }
    /// Nonblocking exact-arm recovery. On a lost ARM reply this does not read
    /// the birth map until the actual SCM keeper pidfd is terminal. The caller
    /// retains this whole !Send owner and uses the bounded capability-unit stop
    /// path before retrying; wrapper exit/timeout never authorizes restoration.
    /// A false return cannot be treated as ordinary CLI continuation.
    pub fn creator_recovery_pending(&self) -> bool {
        self.creator_mask.active != 0
    }
    pub fn recover_creator(&mut self) -> io::Result<()> {
        if self.creator_mask.active == 0 {
            return Ok(());
        }
        if let Some(sequence) = self.armed_sequence {
            let map = self
                .creator_map
                .as_ref()
                .ok_or_else(|| io::Error::other("missing creator recovery map"))?;
            let helper = self
                .keeper_task
                .as_ref()
                .ok_or_else(|| io::Error::other("missing actual keeper pidfd"))?;
            let mut observed = NativeBirth::default();
            let result = unsafe {
                ug_creator_terminalize(
                    map.as_raw_fd(),
                    self.parent_pidfd.as_raw_fd(),
                    helper.as_raw_fd(),
                    self.incarnation,
                    sequence,
                    i32::from(self.arm_acknowledged),
                    &mut observed,
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            self.creator_terminal = Some(result);
            if observed.phase == 5 {
                self.birth = Some(GuardBirth::decode(
                    [
                        observed.incarnation,
                        observed.sequence,
                        observed.phase,
                        observed.in_copy,
                        observed.object,
                        observed.generation,
                        observed.cookie,
                        0,
                    ],
                    self.incarnation,
                    sequence,
                )?);
            }
        }
        if unsafe { ug_creator_mask_restore(&mut self.creator_mask, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn arm_for_clone(&mut self, deadline: Instant) -> io::Result<()> {
        use std::os::fd::AsFd;
        if unsafe { libc::syscall(libc::SYS_gettid) } != self.creator_tid as libc::c_long
            || self.armed_sequence.is_some()
            || self.creator_mask.active != 0
        {
            return Err(io::Error::other("guard creator thread/arm mismatch"));
        }
        let sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("guard sequence exhausted"))?;
        let creator = self.parent_pidfd.try_clone()?;
        if unsafe { ug_creator_mask_block(&mut self.creator_mask) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // Install unknown-command ownership BEFORE the possibly successful
        // send. There is no await or callback between ARM and Container clone.
        self.armed_sequence = Some(sequence);
        let reply = self.request(ARM, 0, &[creator.as_fd()], deadline)?;
        if !reply.rights.is_empty() {
            self.unresolved_rights.extend(reply.rights);
            return Err(io::Error::other("unexpected ARM rights"));
        }
        self.arm_acknowledged = true;
        Ok(())
    }
    pub fn birth(&self) -> Option<GuardBirth> {
        self.birth
    }
    /// Observe only the exact launcher child's wait status. This is not the
    /// helper's lifetime: sudo/systemd-run can be the direct child. Even raw0 could not prove
    /// pinned policy removal, socket retirement or a successful guest result.
    pub fn poll_launcher_terminal(&mut self) -> io::Result<Option<i32>> {
        self.launcher.poll_wait()
    }
    /// Read the typed primary before classifying any startup/keeper failure.
    /// Never use wait status 122/125 or a text match to choose Policy.
    pub fn observe_guard(&mut self) -> GuardOutcome {
        if let Some(receipt) = self.terminal {
            return receipt.outcome;
        }
        let Some(keeper) = self.keeper_task.as_ref() else {
            if self.monitor.primary == 0 {
                self.monitor.primary = 2;
            }
            self.monitor.secondary_monitor_failures |= 2; // UG_MONITOR_CONFIG
            return if self.monitor.primary == 1 {
                GuardOutcome::Policy(self.monitor)
            } else {
                GuardOutcome::Internal(self.monitor)
            };
        };
        if self.readers.len() != 3 {
            if self.monitor.primary == 0 {
                self.monitor.primary = 2;
            }
            self.monitor.secondary_monitor_failures |= 2; // UG_MONITOR_CONFIG
            return if self.monitor.primary == 1 {
                GuardOutcome::Policy(self.monitor)
            } else {
                GuardOutcome::Internal(self.monitor)
            };
        }
        observe(
            MonitorFds {
                config: self.readers[0].as_raw_fd(),
                status: self.readers[1].as_raw_fd(),
                ring: self.readers[2].as_raw_fd(),
                other_actor_pidfd: keeper.as_raw_fd(),
                incarnation: self.incarnation,
            },
            &mut self.monitor,
        )
    }
}
/// Exact helper-side aggregate proof. It proves terminal membership, no live
/// owned sockets/namespaces, link-ID absence and removal of the owned pin leaf.
/// Parent read-only map handles are explicitly closed after final typed read;
/// process/unit terminal observation is still required by the aggregate caller.
#[derive(Clone, Copy, Debug)]
pub struct GuardTerminalReceipt {
    pub incarnation: u64,
    pub proof_sequence: u64,
    pub reply_sequence: u64,
    pub record_ordinal: u64,
    pub removed_links: u64,
    pub removed_map_pins: u64,
    pub initial_tasks: u64,
    pub outcome: GuardOutcome,
}
impl ParentGuard {
    /// Issue one bounded parent-lane attempt after finalizing the actual
    /// Container result. Busy/failed proof retains this entire owner and pins.
    /// A later attempt uses a new sequence; it never recreates admission state.
    pub fn request_terminal(&mut self, deadline: Instant) -> io::Result<GuardTerminalReceipt> {
        if let Some(receipt) = self.terminal {
            return Ok(receipt);
        }
        self.recover_creator()?;
        let first = *self.terminal_sequence.get_or_insert(
            self.next_sequence
                .checked_add(1)
                .ok_or_else(|| io::Error::other("guard sequence exhausted"))?,
        );
        let (v, reply_sequence) = if let Some(proof) = self.terminal_proof {
            proof
        } else {
            if self.terminal_in_flight {
                return Err(io::Error::other(
                    "unknown submitted guard terminal outcome retained",
                ));
            }
            self.terminal_in_flight = true;
            let reply = match self.request(TERMINAL, 0, &[], deadline) {
                Ok(reply) => reply,
                Err(error) => {
                    // Only an authenticated negative reply permits a new read/
                    // cleanup attempt. Timeout/protocol loss remains submitted.
                    if self.last_request_verified {
                        self.terminal_in_flight = false;
                    }
                    return Err(error);
                }
            };
            if !reply.rights.is_empty() {
                self.unresolved_rights.extend(reply.rights);
                return Err(io::Error::other("unexpected terminal rights"));
            }
            let v = reply.frame.values;
            if v[0] != self.incarnation
                || v[1] < first
                || v[1] > reply.frame.sequence
                || v[2] == 0
                || v[3] > 31
                || v[4] > 10
                || v[5] > 256
                || v[6] > 2
                || v[7] != 0
                || (self.readers.len() == 3 && (v[3] != 31 || v[4] != 10))
            {
                return Err(io::Error::other("invalid aggregate guard terminal receipt"));
            }
            let proof = (v, reply.frame.sequence);
            self.terminal_proof = Some(proof); // before any fallible final read
            self.terminal_in_flight = false;
            proof
        };
        // The helper may already have exited after its reply. This locked
        // snapshot preserves the primary without classifying expected exit as
        // failure. Held maps remain valid after object/link cleanup.
        let outcome = if self.readers.len() == 3 {
            let fds = MonitorFds {
                config: self.readers[0].as_raw_fd(),
                status: self.readers[1].as_raw_fd(),
                ring: self.readers[2].as_raw_fd(),
                other_actor_pidfd: -1,
                incarnation: self.incarnation,
            };
            let result = unsafe { ug_monitor_snapshot(&fds, &mut self.monitor) };
            if result != 0 {
                return Err(io::Error::other("incomplete final guard status"));
            }
            if self.monitor.primary == 1 {
                GuardOutcome::Policy(self.monitor)
            } else if self.monitor.primary == 2 {
                GuardOutcome::Internal(self.monitor)
            } else {
                GuardOutcome::Running
            }
        } else {
            // Partial startup lacks complete readers. Cleanup may still be
            // proved, but the original startup failure remains internal.
            if self.monitor.primary == 0 {
                self.monitor.primary = 2;
            }
            GuardOutcome::Internal(self.monitor)
        };
        let receipt = GuardTerminalReceipt {
            incarnation: v[0],
            proof_sequence: v[1],
            reply_sequence,
            record_ordinal: v[2],
            removed_links: v[3],
            removed_map_pins: v[4],
            initial_tasks: v[5],
            outcome,
        };
        self.terminal = Some(receipt); // durable before explicit owner closes
        self.readers.clear();
        drop(self.creator_map.take());
        drop(self.controller_channel.take());
        drop(self.channel.take());
        Ok(receipt)
    }
    /// Exact helper identity, not launcher MainPID. No blocking wait is hidden
    /// here, and true is not a substitute for request_terminal's policy proof.
    pub fn keeper_is_terminal(&self) -> io::Result<bool> {
        let fd = self
            .keeper_task
            .as_ref()
            .ok_or_else(|| io::Error::other("missing actual keeper pidfd"))?;
        let mut p = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut p, 1, 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if p.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::Error::other("keeper pidfd poll failure"));
        }
        Ok(n > 0 && p.revents & libc::POLLIN != 0)
    }
}
#[must_use]
pub struct PrepareFailure {
    pub error: io::Error,
    pub owner: Option<ParentGuard>,
}
/// All copied state consists of actual FD owners/plain counters. !Send prevents
/// preparing on one task and invoking the synchronous clone on another thread.
pub struct PreparedGuard {
    parent: Cell<Option<ParentGuard>>,
    _same_thread: PhantomData<*mut ()>,
}
pub struct ControllerGuard {
    channel: OwnedFd,
    config: OwnedFd,
    status: OwnedFd,
    ring: OwnedFd,
    keeper_pidfd: OwnedFd,
    parent_pidfd: OwnedFd,
    incarnation: u64,
    next_sequence: u64,
    armed_sequence: u64,
    birth: Option<GuardBirth>,
    unresolved_rights: Vec<OwnedFd>,
    monitor: GuardEvidence,
}
impl ControllerGuard {
    /// This must be called only with the actual backend's authenticated held
    /// pidfd for a stopped initial guest, before that task is permitted to run.
    /// The generic GlobalRPC Tid is deliberately not an input.
    pub unsafe fn register_stopped_initial(
        &mut self,
        pidfd: BorrowedFd<'_>,
        deadline: Instant,
    ) -> io::Result<()> {
        use std::os::fd::AsFd;
        // This method runs after original STARTUP_READY, with the authentic
        // initial task still stopped. Parent's post-clone Rust bytes are not
        // magically shared: the child independently reads the same held
        // creator's committed receipt through the inherited command capability.
        self.authenticate_birth(deadline)?;
        let sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("guard sequence exhausted"))?;
        self.next_sequence = sequence;
        match raw_request(
            self.channel.as_fd(),
            self.incarnation,
            sequence,
            INITIAL,
            0,
            &[pidfd],
            deadline,
        ) {
            Ok(reply) if reply.rights.is_empty() => Ok(()),
            Ok(reply) => {
                self.unresolved_rights.extend(reply.rights);
                Err(io::Error::other("unexpected initial-registration rights"))
            }
            Err(failure) => {
                self.unresolved_rights.extend(failure.rights);
                Err(failure.error)
            }
        }
    }
    pub fn birth(&self) -> Option<GuardBirth> {
        self.birth
    }
    fn authenticate_birth(&mut self, deadline: Instant) -> io::Result<()> {
        use std::os::fd::AsFd;
        if self.birth.is_some() {
            return Ok(());
        }
        let sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("guard sequence exhausted"))?;
        self.next_sequence = sequence;
        match raw_request(
            self.channel.as_fd(),
            self.incarnation,
            sequence,
            BIRTH,
            self.armed_sequence,
            &[],
            deadline,
        ) {
            Ok(reply) => {
                if !reply.rights.is_empty() {
                    self.unresolved_rights.extend(reply.rights);
                    return Err(io::Error::other("unexpected child birth rights"));
                }
                self.birth = Some(GuardBirth::decode(
                    reply.frame.values,
                    self.incarnation,
                    self.armed_sequence,
                )?);
                Ok(())
            }
            Err(failure) => {
                self.unresolved_rights.extend(failure.rights);
                Err(failure.error)
            }
        }
    }
    // The independently qualified monitor consumes these held capabilities;
    // it must be started post-clone and joined to the existing typed controller
    // abort before activation. Readiness staging is still a separate join.
    pub fn monitor_handles(
        &self,
    ) -> (
        BorrowedFd<'_>,
        BorrowedFd<'_>,
        BorrowedFd<'_>,
        BorrowedFd<'_>,
        BorrowedFd<'_>,
        u64,
    ) {
        use std::os::fd::AsFd;
        (
            self.config.as_fd(),
            self.status.as_fd(),
            self.ring.as_fd(),
            self.keeper_pidfd.as_fd(),
            self.parent_pidfd.as_fd(),
            self.incarnation,
        )
    }
    /// Locked map snapshot without a native wait. This does not authorize
    /// guest copyout or certify any helper/task/socket terminal state.
    pub fn snapshot_guard(&mut self) -> GuardOutcome {
        let fds = MonitorFds {
            config: self.config.as_raw_fd(),
            status: self.status.as_raw_fd(),
            ring: self.ring.as_raw_fd(),
            other_actor_pidfd: self.keeper_pidfd.as_raw_fd(),
            incarnation: self.incarnation,
        };
        let result = unsafe { ug_monitor_snapshot(&fds, &mut self.monitor) };
        match self.monitor.primary {
            1 => GuardOutcome::Policy(self.monitor),
            2 => GuardOutcome::Internal(self.monitor),
            0 if result > 0 => GuardOutcome::Pending,
            0 if result == 0 => GuardOutcome::Running,
            _ => {
                self.monitor.primary = 2;
                GuardOutcome::Internal(self.monitor)
            }
        }
    }

    /// Driven by a post-clone independent observer, not guest scheduler turns.
    /// The caller must route Policy through the existing typed controlled-abort
    /// boundary and retain this owner on errors. This method never publishes or
    /// authorizes guest poll/epoll copyout.
    pub fn observe_guard(&mut self) -> GuardOutcome {
        observe(
            MonitorFds {
                config: self.config.as_raw_fd(),
                status: self.status.as_raw_fd(),
                ring: self.ring.as_raw_fd(),
                other_actor_pidfd: self.keeper_pidfd.as_raw_fd(),
                incarnation: self.incarnation,
            },
            &mut self.monitor,
        )
    }
}

/// # Safety
/// Invoke before threads, with no competing child reaper. The supplied roots
/// are private externally recoverable locations; elf is the reviewed immutable
/// guard object. This creates only an outside exec helper, never a guest task.
pub unsafe fn prepare_guard(
    launch: &CapabilityUnitLaunch<'_>,
    stdout: BorrowedFd<'_>,
    stderr: BorrowedFd<'_>,
    elf: OwnedFd,
    bpffs_root: OwnedFd,
    recovery_root: OwnedFd,
    incarnation: u64,
    deadline: Instant,
) -> Result<PreparedGuard, PrepareFailure> {
    use std::os::fd::AsFd;
    let before = |error| PrepareFailure { error, owner: None };
    if incarnation == 0 {
        return Err(before(io::Error::other("zero guard incarnation")));
    }
    if launch.kind != CapabilityServiceKind::UnixGuard {
        return Err(before(io::Error::other("wrong keeper launch purpose")));
    }
    // Shared argv validation owns privilege/limits/environment policy. The
    // prebuilt CString vectors and raw fork stub retain no Command/Child,
    // dlopen owner, Arc, service thread or runtime mutex across Container clone.
    let exe = CString::new(CAPABILITY_SUDO).expect("constant executable");
    let args = launch.arguments().map_err(before)?;
    let mut arg_strings = Vec::with_capacity(args.len() + 1);
    arg_strings.push(exe.clone());
    for value in &args {
        arg_strings.push(
            CString::new(value.as_os_str().as_bytes())
                .map_err(|_| before(io::Error::other("keeper argv NUL")))?,
        );
    }
    let mut argv: Vec<_> = arg_strings.iter().map(|s| s.as_ptr()).collect();
    argv.push(std::ptr::null());
    let env_strings: Vec<_> = CAPABILITY_ENVIRONMENT
        .iter()
        .map(|(key, value)| CString::new(format!("{key}={value}")).expect("constant environment"))
        .collect();
    let mut env: Vec<_> = env_strings.iter().map(|s| s.as_ptr()).collect();
    env.push(std::ptr::null());
    // Duplicates above stdio make dup2 ordering independent of the caller's
    // actual FD numbers. Original bounded-log owners remain with the caller.
    let out = duplicate_above_stdio(stdout).map_err(before)?;
    let err = duplicate_above_stdio(stderr).map_err(before)?;
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) } < 0 {
        return Err(before(io::Error::last_os_error()));
    }
    if action.sa_sigaction == libc::SIG_IGN || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(before(io::Error::other(
            "keeper needs exclusive wait ownership",
        )));
    }
    let mut raw = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
            raw.as_mut_ptr(),
        )
    } < 0
    {
        return Err(before(io::Error::last_os_error()));
    }
    let parent = unsafe { OwnedFd::from_raw_fd(raw[0]) };
    let helper_original = unsafe { OwnedFd::from_raw_fd(raw[1]) };
    let helper_endpoint = duplicate_above_stdio(helper_original.as_fd()).map_err(before)?;
    drop(helper_original);
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    let self_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, tid, PIDFD_THREAD) };
    if self_fd < 0 {
        return Err(before(io::Error::last_os_error()));
    }
    let parent_pidfd = unsafe { OwnedFd::from_raw_fd(self_fd as i32) };
    // Raw fork avoids atfork handlers. The child uses only async-signal-safe
    // dup/close/exec/_exit and never constructs or drops parent Rust owners.
    let pid = unsafe { libc::syscall(libc::SYS_fork) };
    if pid < 0 {
        return Err(before(io::Error::last_os_error()));
    }
    if pid == 0 {
        unsafe {
            libc::close(parent.as_raw_fd());
            if libc::dup2(helper_endpoint.as_raw_fd(), 0) < 0
                || libc::dup2(out.as_raw_fd(), 1) < 0
                || libc::dup2(err.as_raw_fd(), 2) < 0
            {
                libc::_exit(125);
            }
            libc::close(helper_endpoint.as_raw_fd());
            libc::close(out.as_raw_fd());
            libc::close(err.as_raw_fd());
            libc::execve(exe.as_ptr(), argv.as_ptr(), env.as_ptr());
            libc::_exit(125);
        }
    }
    drop(helper_endpoint);
    drop(out);
    drop(err);
    let mut owner = ParentGuard {
        unit: launch.unit.to_owned(),
        outside: Some(OutsideGuardResources {
            elf,
            bpffs_root,
            recovery_root,
        }),
        launcher: LauncherChild {
            pid: pid as i32,
            pidfd: None,
            status: None,
        },
        keeper_task: None,
        channel: Some(parent),
        controller_channel: None,
        terminal: None,
        terminal_sequence: None,
        terminal_in_flight: false,
        terminal_proof: None,
        last_request_verified: false,
        parent_pidfd,
        readers: Vec::new(),
        unresolved_rights: Vec::new(),
        controller: None,
        incarnation,
        next_sequence: 0,
        armed_sequence: None,
        birth: None,
        creator_tid: tid as i32,
        creator_map: None,
        creator_mask: CreatorMask::default(),
        arm_acknowledged: false,
        creator_terminal: None,
        creator_recovery_error: None,
        first_startup_error: None,
        _same_thread: PhantomData,
        monitor: GuardEvidence::default(),
    };
    // The direct launcher child cannot be reaped by anyone else under this
    // constructor's contract. This pidfd is NOT helper identity once the shared
    // privilege launcher replaces direct exec; INIT supplies the helper pidfd.
    let held = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if held < 0 {
        return Err(PrepareFailure {
            error: io::Error::last_os_error(),
            owner: Some(owner),
        });
    }
    owner.launcher.pidfd = Some(unsafe { OwnedFd::from_raw_fd(held as i32) });
    let result = (|| {
        // Borrow a duplicate to avoid aliasing the mutable request owner. The
        // duplicate is closed only after the SCM send has returned.
        let creator = owner.parent_pidfd.try_clone()?;
        let outside = owner
            .outside
            .as_ref()
            .expect("outside resources retained before spawn");
        let elf = outside.elf.try_clone()?;
        let bpffs_root = outside.bpffs_root.try_clone()?;
        let recovery_root = outside.recovery_root.try_clone()?;
        let mut init = owner.request(
            INIT,
            0,
            &[
                elf.as_fd(),
                bpffs_root.as_fd(),
                recovery_root.as_fd(),
                creator.as_fd(),
            ],
            deadline,
        )?;
        if init.rights.len() != 4 {
            owner.unresolved_rights.extend(init.rights);
            return Err(io::Error::other("guard INIT helper identity/readers"));
        }
        owner.keeper_task = Some(init.rights.remove(0));
        owner.readers = init.rights;
        let mut recovery = owner.request(CREATOR_RECOVERY, 0, &[], deadline)?;
        if recovery.rights.len() != 1 {
            owner.unresolved_rights.extend(recovery.rights);
            return Err(io::Error::other("creator recovery capability count"));
        }
        owner.creator_map = Some(recovery.rights.remove(0));
        // This second private lane is created outside the future owned netns.
        // Parent retains the original command lane through aggregate cleanup;
        // only the new endpoint crosses to the controller after COW split.
        let mut raw = [-1; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                raw.as_mut_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        owner.controller_channel = Some(unsafe { OwnedFd::from_raw_fd(raw[0]) });
        let helper_lane = unsafe { OwnedFd::from_raw_fd(raw[1]) };
        let reply = owner.request(CONTROLLER_CHANNEL, 0, &[helper_lane.as_fd()], deadline)?;
        if !reply.rights.is_empty() {
            owner.unresolved_rights.extend(reply.rights);
            return Err(io::Error::other("unexpected controller-lane rights"));
        }
        // No ARM in preparation: caller may fail or defer before clone without
        // leaving an admission command on an ordinary signal-handler path.
        Ok(())
    })();
    match result {
        Ok(()) => Ok(PreparedGuard {
            parent: Cell::new(Some(owner)),
            _same_thread: PhantomData,
        }),
        Err(error) => Err(PrepareFailure {
            error,
            owner: Some(owner),
        }),
    }
}

#[must_use]
pub struct GuardedContainerRun<T> {
    pub container: Result<OwnedDeferredContainerRun<T>, StartupOwnedFailure<T>>,
    pub guard: ParentGuard,
}
/// Before-clone failure retaining exact helper/map/mask recovery. There is no
/// destructor restoring the mask or detaching policy. The maintained caller
/// must settle it using the same finite service-unit recovery bound, or stop
/// the CLI with its externally pinned recovery record; never continue work.
#[must_use]
pub struct GuardStartFailure {
    pub error: io::Error,
    pub owner: ParentGuard,
}
/// Callback parts for ONE existing Container startup exchange. Every owner is
/// a plain COW capability until the actual child callback constructs its runtime.
/// The caller must consume this object with `into_parent` on every parent path;
/// a pending creator mask is an ownership-bearing terminal obligation.
#[must_use]
pub struct ArmedGuard {
    state: Cell<Option<ParentGuard>>,
    parent_result: Cell<Option<ParentGuard>>,
    deadline: Instant,
    _same_thread: PhantomData<*mut ()>,
}
impl PreparedGuard {
    /// Consume an unarmed preparation after another before-clone step failed.
    /// The returned owner still requires actual helper/pin terminal recovery;
    /// dropping this value is not evidence of policy cleanup.
    pub fn into_parent(self) -> ParentGuard {
        self.parent.take().expect("one-use prepared guard")
    }
    /// # Safety
    /// Same creator/fork/no-reaper contract as `prepare_guard`. Call immediately
    /// before the sole Container clone, after other fallible preparation.
    pub unsafe fn arm(self, timeout: Duration) -> Result<ArmedGuard, GuardStartFailure> {
        let mut owner = self.parent.take().expect("one-use prepared guard");
        let deadline = match Instant::now()
            .checked_add(timeout)
            .filter(|_| !timeout.is_zero())
        {
            Some(deadline) => deadline,
            None => {
                return Err(GuardStartFailure {
                    error: io::Error::other("invalid guard clone timeout"),
                    owner,
                });
            }
        };
        if let Err(error) = owner.arm_for_clone(deadline) {
            if let Err(secondary) = owner.recover_creator() {
                owner.creator_recovery_error = Some(secondary);
            }
            return Err(GuardStartFailure { error, owner });
        }
        Ok(ArmedGuard {
            state: Cell::new(Some(owner)),
            parent_result: Cell::new(None),
            deadline,
            _same_thread: PhantomData,
        })
    }
}
impl ArmedGuard {
    /// The original startup budget, reduced by actual ARM time.
    pub fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|v| !v.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "guard ARM used startup deadline")
            })
    }
    /// Called in the outside parent callback, before STARTUP_READY. `controller`
    /// is the actual Container-owned child pidfd, not a reported PID or wrapper.
    pub fn parent_startup(
        &self,
        controller: BorrowedFd<'_>,
        transferred_descriptors: usize,
        deadline: Instant,
    ) -> Result<(), StartupError> {
        let mut parent = self.state.take().ok_or(StartupError::Protocol)?;
        // Install parent recovery before any fallible receipt or duplicate.
        let admission = (|| {
            parent.controller = Some(
                controller
                    .try_clone_to_owned()
                    .map_err(|_| StartupError::Protocol)?,
            );
            if transferred_descriptors != 0 {
                return Err(StartupError::Protocol);
            }
            let controller = parent
                .controller
                .as_ref()
                .ok_or(StartupError::Protocol)?
                .try_clone()
                .map_err(|_| StartupError::Protocol)?;
            use std::os::fd::AsFd;
            let reply = parent
                .request(CONTROLLER_TASK, 0, &[controller.as_fd()], deadline)
                .map_err(|error| {
                    parent.first_startup_error = Some(error);
                    StartupError::Protocol
                })?;
            if !reply.rights.is_empty() {
                parent.unresolved_rights.extend(reply.rights);
                return Err(StartupError::Protocol);
            }
            let armed = parent.armed_sequence.ok_or(StartupError::Protocol)?;
            let birth = parent
                .request(BIRTH, armed, &[], deadline)
                .map_err(|error| {
                    parent.first_startup_error = Some(error);
                    StartupError::Protocol
                })?;
            if !birth.rights.is_empty() {
                parent.unresolved_rights.extend(birth.rights);
                return Err(StartupError::Protocol);
            }
            // Exact command result has already validated committed pointer/
            // generation/cookie; retain its bytes for later task/ns joins.
            parent.birth = Some(
                GuardBirth::decode(birth.frame.values, parent.incarnation, armed).map_err(
                    |error| {
                        parent.first_startup_error = Some(error);
                        StartupError::Protocol
                    },
                )?,
            );
            // Exact arm is now inert. Restore outside creator signals
            // before STARTUP_READY, never after a workload-length drain.
            parent.recover_creator().map_err(|error| {
                parent.creator_recovery_error = Some(error);
                StartupError::Protocol
            })?;
            // Parent keeps its distinct command lane for aggregate
            // terminal cleanup. Close only its COW controller-lane alias.
            drop(parent.controller_channel.take());
            Ok(())
        })();
        self.parent_result.set(Some(parent));
        admission
    }
    /// # Safety
    /// Called only in this ArmedGuard's actual fork child before backend/guest
    /// execution. Explicit closes affect this branch's actual COW owners only.
    pub unsafe fn child_startup(&self) -> Result<ControllerGuard, StartupError> {
        // COW copy contains only explicit plain FD owners and counters.
        // Taking them constructs the child owner after clone; the parent
        // copy is unchanged. No libbpf object or active thread was copied.
        let mut copied = self.state.take().ok_or(StartupError::Protocol)?;
        // The outside-only writable map FD must not reach the backend or
        // guest. Close the child's actual inherited owner explicitly.
        drop(copied.creator_map.take());
        drop(copied.outside.take());
        // Restore the original inherited mask even on a later child setup
        // failure. This COW obligation is distinct from the parent arm.
        if unsafe { ug_creator_mask_restore(&mut copied.creator_mask, 1) } < 0 {
            return Err(StartupError::Protocol);
        }
        if copied.readers.len() != 3 {
            return Err(StartupError::Protocol);
        }
        let ring = copied.readers.pop().unwrap();
        let status = copied.readers.pop().unwrap();
        let config = copied.readers.pop().unwrap();
        drop(copied.channel.take()); // outside-parent lane must not reach guest
        let channel = copied
            .controller_channel
            .take()
            .ok_or(StartupError::Protocol)?;
        let keeper_pidfd = copied.keeper_task.take().ok_or(StartupError::Protocol)?;
        // No waitpid/Drop of a copied wait guard. LauncherChild is plain
        // identity; only the outside parent is its actual kernel parent.
        Ok(ControllerGuard {
            channel,
            config,
            status,
            ring,
            keeper_pidfd,
            parent_pidfd: copied.parent_pidfd,
            incarnation: copied.incarnation,
            next_sequence: 0, // independent authenticated controller lane
            armed_sequence: copied.armed_sequence.ok_or(StartupError::Protocol)?,
            birth: None,
            unresolved_rights: copied.unresolved_rights,
            monitor: copied.monitor,
        })
    }
    /// Consume in the outside parent, including before-clone and child-startup
    /// failure. Recovery failure remains recorded on the returned actual owner;
    /// it cannot be flattened into a string and discarded.
    pub fn into_parent(self) -> ParentGuard {
        let mut guard = self
            .parent_result
            .take()
            .or_else(|| self.state.take())
            .expect("before-clone failure or parent callback retains exact guard owner");
        if let Err(error) = guard.recover_creator() {
            guard.creator_recovery_error = Some(error);
        }
        guard
    }
}
/// # Safety
/// Same fork/no-reaper contract as prepare_guard and Container. The `run`
/// callback must retain ControllerGuard outside its cancellable backend future.
/// This compatibility wrapper delegates to the same exposed callback parts;
/// the maintained combined network path must use ONE Container exchange.
pub unsafe fn run_guarded<T, U, F>(
    container: &mut Container,
    prepared: PreparedGuard,
    timeout: Duration,
    run: &mut F,
) -> Result<GuardedContainerRun<T>, GuardStartFailure>
where
    T: serde::Serialize,
    F: FnMut(ControllerGuard) -> (T, U),
{
    let armed = unsafe { prepared.arm(timeout) }?;
    let remaining = match armed.remaining() {
        Ok(remaining) => remaining,
        Err(error) => {
            return Err(GuardStartFailure {
                error,
                owner: armed.into_parent(),
            });
        }
    };
    let result = container.run_with_startup_owned(
        remaining,
        &mut |context| {
            armed.parent_startup(
                context.child_pidfd(),
                context.descriptor_count(),
                context.deadline(),
            )
        },
        &mut |_context| unsafe { armed.child_startup() },
        run,
    );
    Ok(GuardedContainerRun {
        container: result,
        guard: armed.into_parent(),
    })
}

#[cfg(test)]
mod birth_tests {
    use super::GuardBirth;
    const VALID: [u64; 8] = [7, 2, 5, 0, 0x1234, 11, 29, 0];
    #[test]
    fn retains_exact_committed_birth_not_only_incarnation() {
        assert_eq!(
            GuardBirth::decode(VALID, 7, 2).unwrap(),
            GuardBirth {
                incarnation: 7,
                sequence: 2,
                object: 0x1234,
                generation: 11,
                cookie: 29
            }
        );
    }
    #[test]
    fn rejects_every_mismatched_or_uncommitted_birth_field() {
        for (index, value) in [
            (0, 8),
            (1, 3),
            (2, 4),
            (3, 1),
            (4, 0),
            (5, 0),
            (6, 0),
            (7, 1),
        ] {
            let mut input = VALID;
            input[index] = value;
            assert!(GuardBirth::decode(input, 7, 2).is_err(), "field {index}");
        }
        assert!(GuardBirth::decode(VALID, 0, 2).is_err());
        assert!(GuardBirth::decode(VALID, 7, 0).is_err());
    }
}
