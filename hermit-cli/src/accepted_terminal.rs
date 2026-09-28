//! Existing accepted-service ownership through exact close and external readback.
//! Guest output cannot be released from this boundary on counts or EOF alone.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::io::{self};
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::rc::Rc;
use std::time::Instant;

use detcore::network_runtime::AcceptedPostSpawn;
use detcore::network_runtime::ParentAcceptedService;
use detcore::network_runtime::ProviderArtifact;
use detcore::network_runtime::capability_unit::CAPABILITY_ENVIRONMENT;
use detcore::network_runtime::capability_unit::CAPABILITY_SUDO;
use detcore::network_runtime::capability_unit::CapabilityServiceKind;
use detcore::network_runtime::capability_unit::CapabilityServiceLifetime;
use detcore::network_runtime::capability_unit::CapabilityUnitLaunch;
use process::CommandFlight;
use process::UnitIdentity;
use process::group_absent;
use process::pause;
use process::within;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::unix_guard_terminal::process;

const LOG_BYTES: u64 = 1_048_576;
const QUERY_BYTES: u64 = 8192;
const CLOSE_NS: u64 = 1_000_000_000;
// Same aggregate cleanup interval as network_run::TERMINAL. It starts on the
// actual controller PIDFD terminal observation, never on a received EOF/query.
const CONTROLLER_CLEANUP_NS: u64 = 30 * CLOSE_NS;

fn pidfd_terminal(fd: BorrowedFd<'_>) -> io::Result<bool> {
    let mut item = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut item, 1, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if item.revents & (libc::POLLNVAL | libc::POLLERR) != 0 {
        return Err(io::Error::other(
            "invalid retained accepted helper/controller PIDFD",
        ));
    }
    Ok(item.revents & (libc::POLLIN | libc::POLLHUP) != 0)
}

fn send_bootstrap(
    socket: BorrowedFd<'_>,
    controller: BorrowedFd<'_>,
    run: [u8; 16],
) -> io::Result<()> {
    let mut run = run;
    let mut iov = libc::iovec {
        iov_base: run.as_mut_ptr().cast(),
        iov_len: run.len(),
    };
    let mut control = [0usize; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
        std::ptr::write_unaligned(
            libc::CMSG_DATA(header).cast::<i32>(),
            controller.as_raw_fd(),
        );
    }
    if unsafe {
        libc::sendmsg(
            socket.as_raw_fd(),
            &msg,
            libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
        )
    } != 16
    {
        return Err(io::Error::other(
            "accepted readback bootstrap not sent exactly once",
        ));
    }
    Ok(())
}
fn receive_bootstrap(socket: BorrowedFd<'_>) -> io::Result<([u8; 16], OwnedFd)> {
    let mut run = [0u8; 16];
    let mut iov = libc::iovec {
        iov_base: run.as_mut_ptr().cast(),
        iov_len: run.len(),
    };
    let mut control = [0usize; 132]; // all SCM_MAX_FD rights, including malformed excess
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control);
    let received = unsafe {
        libc::recvmsg(
            socket.as_raw_fd(),
            &mut msg,
            libc::MSG_CMSG_CLOEXEC | libc::MSG_TRUNC | libc::MSG_DONTWAIT,
        )
    };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut rights = Vec::new();
    let mut invalid = false;
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            let len = (*header)
                .cmsg_len
                .saturating_sub(libc::CMSG_LEN(0) as usize);
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                invalid |= len % std::mem::size_of::<i32>() != 0;
                for offset in 0..len / std::mem::size_of::<i32>() {
                    let raw =
                        std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<i32>().add(offset));
                    rights.push(OwnedFd::from_raw_fd(raw));
                }
            } else {
                invalid = true;
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
    }
    if received != 16
        || run == [0; 16]
        || invalid
        || rights.len() != 1
        || msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
    {
        return Err(io::Error::other(
            "accepted readback bootstrap changed run/controller rights",
        ));
    }
    let controller = rights.remove(0);
    pidfd_terminal(controller.as_fd())?;
    Ok((run, controller))
}

// Convert the already-owned startup deadline once, before launching the
// helper. Sampling CLOCK_MONOTONIC first makes clock-sampling latency shorten
// this bound; no attempt receives a fresh relative bootstrap interval.
fn bootstrap_deadline_ns(deadline: Instant) -> io::Result<u64> {
    let observed = now_ns()?;
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "accepted bootstrap deadline expired before launch",
            )
        })?;
    observed
        .checked_add(u64::try_from(remaining.as_nanos()).map_err(io::Error::other)?)
        .ok_or_else(|| io::Error::other("accepted bootstrap deadline overflow"))
}

fn receive_bootstrap_until(
    socket: BorrowedFd<'_>,
    deadline_ns: u64,
) -> io::Result<([u8; 16], OwnedFd)> {
    loop {
        if now_ns()? >= deadline_ns {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "original accepted bootstrap deadline",
            ));
        }
        // Nonblocking receive also handles EOF or a packet already queued at
        // entry. A poll wake never authorizes a renewed deadline or blocking
        // receive, including on EINTR/spurious readiness.
        match receive_bootstrap(socket) {
            Ok(value) => {
                if now_ns()? >= deadline_ns {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "accepted bootstrap completed after original deadline",
                    ));
                }
                return Ok(value);
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        let remaining = deadline_ns
            .checked_sub(now_ns()?)
            .filter(|ns| *ns != 0)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "original accepted bootstrap deadline",
                )
            })?;
        let timeout = libc::timespec {
            tv_sec: (remaining / CLOSE_NS) as libc::time_t,
            tv_nsec: (remaining % CLOSE_NS) as libc::c_long,
        };
        let mut item = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::ppoll(&mut item, 1, &timeout, std::ptr::null()) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

fn now_ns() -> io::Result<u64> {
    let mut clock: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) } < 0 {
        return Err(io::Error::last_os_error());
    }
    (clock.tv_sec as u64)
        .checked_mul(CLOSE_NS)
        .and_then(|v| v.checked_add(clock.tv_nsec as u64))
        .ok_or_else(|| io::Error::other("accepted terminal clock overflow"))
}
/// Keep the actual service transcript readable after the inherited writer
/// closes. Both descriptions retain this single O_RDWR open-file description.
pub fn create_retained_service_log(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
}

// Both calls use a fresh observation of the same retained controller PIDFD.
// In particular, query readiness from poll cannot bypass the terminal check
// when the controller exits between the pre-poll observation and that wake.
fn observe_readback_wake(
    terminal: bool,
    query_ready: bool,
    observed_ns: u64,
    deadline: &mut Option<u64>,
) -> io::Result<bool> {
    if terminal && deadline.is_none() {
        *deadline = Some(
            observed_ns
                .checked_add(CONTROLLER_CLEANUP_NS)
                .ok_or_else(|| io::Error::other("accepted helper deadline overflow"))?,
        );
    }
    if deadline.is_some_and(|limit| observed_ns >= limit) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "accepted controller cleanup expired before query",
        ));
    }
    if query_ready && !terminal {
        return Err(io::Error::other(
            "accepted query arrived before original controller terminal",
        ));
    }
    Ok(query_ready)
}

fn log(file: &mut File) -> io::Result<String> {
    if file.metadata()?.len() > LOG_BYTES {
        return Err(io::Error::other(
            "accepted log exceeded original unit bound",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = String::new();
    (&mut *file)
        .take(LOG_BYTES + 1)
        .read_to_string(&mut bytes)?;
    if bytes.len() as u64 > LOG_BYTES {
        return Err(io::Error::other("accepted log grew beyond bound"));
    }
    Ok(bytes)
}
fn validate_ids(ids: &[(u32, u32)], counts: [usize; 3]) -> io::Result<()> {
    let mut seen = BTreeSet::new();
    let mut actual = [0; 3];
    for &(kind, id) in ids {
        if kind > 2 || id == 0 || !seen.insert((kind, id)) {
            return Err(io::Error::other(
                "partial, duplicate or invalid accepted original ID",
            ));
        }
        actual[kind as usize] += 1;
    }
    if counts.contains(&0) || actual != counts {
        return Err(io::Error::other(
            "accepted ID inventory differs from authenticated artifact",
        ));
    }
    Ok(())
}
fn inventory(value: &Value) -> io::Result<Vec<(u32, u32)>> {
    if value["complete"] != true
        || value["count_invalid"] != false
        || value["status"]["returned"] != 0
        || !value["status"]["errno"].is_null()
    {
        return Err(io::Error::other(
            "accepted terminal inventory is incomplete",
        ));
    }
    value["ids"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing original IDs"))?
        .iter()
        .map(|id| {
            Ok((
                u32::try_from(
                    id["kind"]
                        .as_u64()
                        .ok_or_else(|| io::Error::other("invalid ID kind"))?,
                )
                .map_err(io::Error::other)?,
                u32::try_from(
                    id["id"]
                        .as_u64()
                        .ok_or_else(|| io::Error::other("invalid original ID"))?,
                )
                .map_err(io::Error::other)?,
            ))
        })
        .collect()
}
fn same_ids(a: &[(u32, u32)], b: &[(u32, u32)]) -> bool {
    a.len() == b.len() && a.iter().copied().collect::<BTreeSet<_>>() == b.iter().copied().collect()
}

/// Parse the actual service transcript after its retained wrapper has exited.
/// Every positive resource must occur in READY, before-close and close custody.
fn closed_inventory(text: &str, run: [u8; 16], ids: &[(u32, u32)]) -> io::Result<u64> {
    let rows: Vec<Value> = text
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    if rows.len() != 2 || rows[0]["phase"] != "before_close" || rows[1]["phase"] != "after_close" {
        return Err(io::Error::other(
            "accepted service transcript contains failure or missing terminal phase",
        ));
    }
    let before = &rows[0];
    let after = &rows[1];
    for row in &rows {
        if row["schema"] != "hermit-accepted-provider-terminal-v1"
            || row["run"] != serde_json::json!(run)
            || row["controller_terminal"] != true
            || row["requires_external_absence"] != true
        {
            return Err(io::Error::other(
                "accepted service terminal changed run/controller identity",
            ));
        }
    }
    let inventories = before["inventories"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing inventories"))?;
    let close = after["close_receipts"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing close receipts"))?;
    if inventories.len() != 1
        || close.len() != 1
        || !before["failure"].is_null()
        || after["service_status"] != 0
        || !after["close_error"].is_null()
        || after["socket_release"] != "pending_process_exit"
    {
        return Err(io::Error::other(
            "accepted service did not complete its original single load",
        ));
    }
    let receipt = &close[0];
    if receipt["incarnation"] != u64::from_le_bytes(run[..8].try_into().unwrap())
        || receipt["close"]["returned"] != 0
        || !receipt["close"]["errno"].is_null()
        || receipt["unexpected_drop"] != false
        || receipt["requires_external_absence"] != true
        || !same_ids(&inventory(&inventories[0])?, ids)
        || !same_ids(&inventory(&receipt["inventory"])?, ids)
    {
        return Err(io::Error::other(
            "accepted close does not cover the exact READY inventory",
        ));
    }
    after["closed_ns"]
        .as_u64()
        .filter(|v| *v != 0)
        .ok_or_else(|| io::Error::other("missing original accepted close time"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    run: [u8; 16],
    ids: Vec<(u32, u32)>,
    counts: [usize; 3],
    closed_ns: u64,
    deadline_ns: u64,
}
impl Query {
    fn validate(&self) -> io::Result<()> {
        validate_ids(&self.ids, self.counts)?;
        if self.run == [0; 16]
            || self.ids.len() > 256
            || self.closed_ns == 0
            || self.closed_ns.checked_add(CLOSE_NS) != Some(self.deadline_ns)
        {
            return Err(io::Error::other("invalid accepted readback request"));
        }
        Ok(())
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Readback {
    request: Query,
    /// The same complete original inventory was absent in both full scans.
    passes_ns: [u64; 2],
}
impl Readback {
    fn validate(&self, query: &Query) -> io::Result<()> {
        if self.request != *query
            || self.passes_ns[0] < query.closed_ns
            || self.passes_ns[1] < self.passes_ns[0]
            || self.passes_ns[1] >= query.deadline_ns
        {
            return Err(io::Error::other(
                "accepted readback changed original inventory or deadline",
            ));
        }
        query.validate()
    }
}

/// Private metadata-only entry. It neither opens a provider nor loads a BPF
/// program. The exact inherited private socket is its only request source.
/// It is invoked before CLI/runtime initialization by the maintained launcher.
pub fn run_private_readback(input: io::Result<Option<File>>, bootstrap_before_ns: u64) -> ! {
    let result = (|| -> io::Result<()> {
        let input = input?.ok_or_else(|| io::Error::other("missing accepted query input"))?;
        let mut ty = 0i32;
        let mut size = std::mem::size_of_val(&ty) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                input.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut ty as *mut i32).cast(),
                &mut size,
            )
        } != 0
            || ty != libc::SOCK_SEQPACKET
            || size as usize != std::mem::size_of_val(&ty)
        {
            return Err(io::Error::other(
                "accepted query input is not the original private seqpacket",
            ));
        }
        let (run, controller) = receive_bootstrap_until(input.as_fd(), bootstrap_before_ns)?;
        let mut terminal_deadline = None;
        // The helper owns only controller custody and its private socket: no
        // provider object references can keep original IDs alive. Parent EOF
        // is a failure; only the actual PIDFD starts the finite cleanup clock.
        loop {
            observe_readback_wake(
                pidfd_terminal(controller.as_fd())?,
                false,
                now_ns()?,
                &mut terminal_deadline,
            )?;
            let mut items = [
                libc::pollfd {
                    fd: input.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if terminal_deadline.is_some() {
                        -1
                    } else {
                        controller.as_raw_fd()
                    },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if unsafe { libc::poll(items.as_mut_ptr(), 2, 20) } < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(error);
            }
            if items[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(io::Error::other("accepted query parent channel terminated"));
            }
            if observe_readback_wake(
                pidfd_terminal(controller.as_fd())?,
                items[0].revents & libc::POLLIN != 0,
                now_ns()?,
                &mut terminal_deadline,
            )? {
                break;
            }
        }
        if !pidfd_terminal(controller.as_fd())? {
            return Err(io::Error::other(
                "accepted query arrived before original controller terminal",
            ));
        }
        let mut bytes = [0u8; QUERY_BYTES as usize + 1];
        let length = unsafe {
            libc::recv(
                input.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                libc::MSG_TRUNC | libc::MSG_DONTWAIT,
            )
        };
        if length <= 0 || length as u64 > QUERY_BYTES {
            return Err(io::Error::other("missing or oversize accepted query"));
        }
        let query: Query = serde_json::from_slice(&bytes[..length as usize])?;
        query.validate()?;
        if query.run != run || terminal_deadline.is_none_or(|deadline| query.deadline_ns > deadline)
        {
            return Err(io::Error::other(
                "accepted query changed initial run/controller deadline",
            ));
        }
        let mut passes = Vec::new();
        loop {
            if now_ns()? >= query.deadline_ns {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "original accepted close deadline",
                ));
            }
            let mut absent = true;
            for &(kind, id) in &query.ids {
                // Linux BPF_*_GET_FD_BY_ID union: u32 id,next_id,open_flags.
                // A zeroed full attr carries no pathname or mutation operation.
                let mut attr = [0u64; 18];
                attr[0] = u64::from(id);
                let command = match kind {
                    0 => 14,
                    1 => 13,
                    2 => 30,
                    _ => unreachable!(),
                };
                let raw = unsafe { libc::syscall(libc::SYS_bpf, command, attr.as_ptr(), 12u32) };
                if raw >= 0 {
                    drop(unsafe { OwnedFd::from_raw_fd(raw as i32) });
                    absent = false;
                } else if io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) {
                    return Err(io::Error::last_os_error());
                }
            }
            let observed = now_ns()?;
            if observed >= query.deadline_ns {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "accepted complete scan exceeded original deadline",
                ));
            }
            if absent {
                passes.push(observed);
            } else {
                passes.clear();
            }
            if passes.len() == 2 {
                let receipt = Readback {
                    request: query,
                    passes_ns: [passes[0], passes[1]],
                };
                let mut out = io::stdout().lock();
                serde_json::to_writer(&mut out, &receipt)?;
                out.write_all(b"\n")?;
                out.flush()?;
                if unsafe { libc::fsync(libc::STDOUT_FILENO) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
        }
    })();
    if let Err(error) = &result {
        eprintln!("accepted metadata readback failed: {error}");
    }
    unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) }
}

#[derive(Clone, Copy)]
enum AcceptedUnitRole {
    Loader,
    Query,
}

/// Parent's actual provider owner, wrapper, command flights, and original logs.
/// On any failure this object must remain retained for exact scoped recovery.
#[must_use = "retain incomplete accepted provider ownership"]
pub struct AcceptedParentFinalizer {
    grouped: Option<Rc<RefCell<detcore::network_runtime::GroupedParentOwner>>>,
    service: Option<ParentAcceptedService>,
    stdout: File,
    stderr: File,
    executable: PathBuf,
    loader: Option<UnitIdentity>,
    loader_task: Option<OwnedFd>,
    query_task: Option<OwnedFd>,
    query_bootstrapped: bool,
    query_aborted: bool,
    query_abort_failure: Option<String>,
    query: Option<(String, CommandFlight, Option<UnitIdentity>, OwnedFd)>,
    commands: Vec<CommandFlight>,
    failed_reset_intents: BTreeSet<String>,
    first_failed_manager_readbacks: BTreeMap<String, String>,
    failed_unit_observations: Vec<Value>,
    deadline: Option<Instant>,
    failure: Option<String>,
}
/// Same-thread handle to the original grouped owner. It cannot reconstruct a
/// Child, native creator, or deadline from a diagnostic record.
#[derive(Clone, Default)]
pub struct GroupedParentHook(Option<Rc<RefCell<detcore::network_runtime::GroupedParentOwner>>>);
impl detcore::network_runtime::AcceptedPostSpawn for GroupedParentHook {
    fn after_spawn(
        &mut self,
        spawned: detcore::network_runtime::AcceptedSpawned<'_>,
    ) -> io::Result<Option<detcore::network_runtime::GroupedBootstrapTransport>> {
        let Some(owner) = &self.0 else {
            return Ok(None);
        };
        owner
            .try_borrow_mut()
            .map_err(|_| io::Error::other("grouped parent owner already borrowed"))?
            .after_spawn(spawned)
    }
    fn progress(&mut self, deadline: Instant) -> io::Result<()> {
        let Some(owner) = &self.0 else {
            return Ok(());
        };
        owner
            .try_borrow_mut()
            .map_err(|_| io::Error::other("grouped parent owner already borrowed"))?
            .progress_owned(deadline)
    }
}

impl AcceptedParentFinalizer {
    /// Retain before clone; native launch begins only through after_spawn.
    pub fn install_grouped(
        &mut self,
        owner: &mut Option<detcore::network_runtime::GroupedParentOwner>,
    ) -> io::Result<()> {
        if self.grouped.is_some() || owner.is_none() {
            return Err(io::Error::other(
                "grouped parent custody absent or already installed",
            ));
        }
        self.grouped = Some(Rc::new(RefCell::new(owner.take().unwrap())));
        Ok(())
    }
    pub fn grouped_hook(&self) -> GroupedParentHook {
        GroupedParentHook(self.grouped.clone())
    }
    fn progress_grouped(&mut self, deadline: Instant) -> io::Result<()> {
        self.grouped_hook().progress(deadline)
    }
    fn require_grouped_completed(&self) -> io::Result<()> {
        if let Some(owner) = &self.grouped {
            if !owner
                .try_borrow()
                .map_err(|_| io::Error::other("grouped parent owner already borrowed"))?
                .completed()
            {
                return Err(io::Error::other(
                    "original grouped children or runtime unit remain unjoined",
                ));
            }
        }
        Ok(())
    }
    pub fn before_startup(stdout: File, stderr: File, executable: PathBuf) -> Self {
        Self {
            grouped: None,
            service: None,
            stdout,
            stderr,
            executable,
            loader: None,
            loader_task: None,
            query_task: None,
            query_bootstrapped: false,
            query_aborted: false,
            query_abort_failure: None,
            query: None,
            commands: Vec::new(),
            failed_reset_intents: BTreeSet::new(),
            failed_unit_observations: Vec::new(),
            first_failed_manager_readbacks: BTreeMap::new(),
            deadline: None,
            failure: None,
        }
    }
    /// Move the original service owner into the already-retained parent observer.
    /// This consumes the pre-startup holder exactly once; no numeric PID lookup.
    pub fn with_service(mut self, service: ParentAcceptedService) -> Self {
        assert!(
            self.service.is_none(),
            "accepted service ownership supplied twice"
        );
        self.service = Some(service);
        self
    }
    /// Snapshot actual startup identities for the separately retained receipt
    /// owner; this does not claim terminality or transfer native authority.
    pub fn startup_receipt(
        &self,
        owner: &crate::network_container::NetworkParentOwnership,
    ) -> io::Result<Value> {
        use crate::network_container::NetworkParentOwnership;
        let service = match owner {
            NetworkParentOwnership::Running(service) => service,
            NetworkParentOwnership::StartupFailed(failure) => &failure.owner,
        };
        self.validate_query_custody()?;
        let loader = self
            .loader
            .as_ref()
            .ok_or_else(|| io::Error::other("accepted loader not captured"))?;
        if loader.unit != service.unit() || self.loader_task.is_none() {
            return Err(io::Error::other("accepted startup loader identity differs"));
        }
        Ok(
            serde_json::json!({"schema":"hermit-accepted-parent-startup-v1","run":service.incarnation(),"artifact":service.expected_artifact(),"loader":loader.receipt(),"query":self.query.as_ref().unwrap().2.as_ref().unwrap().receipt()}),
        )
    }
    fn service(&self) -> io::Result<&ParentAcceptedService> {
        self.service
            .as_ref()
            .ok_or_else(|| io::Error::other("accepted service was never started"))
    }
    fn command(
        &mut self,
        privileged: bool,
        args: &[&str],
        deadline: Instant,
    ) -> io::Result<String> {
        within(deadline)?;
        if self.commands.len() >= 64 {
            return Err(io::Error::other("accepted terminal command bound"));
        }
        let mut command = Command::new(if privileged {
            CAPABILITY_SUDO
        } else {
            "/usr/bin/systemctl"
        });
        command
            .env_clear()
            .envs(CAPABILITY_ENVIRONMENT.iter().copied());
        if privileged {
            command.args(["-n", "/usr/bin/systemctl"]);
        }
        command.args(args);
        self.commands.push(CommandFlight::start(&mut command)?);
        let flight = self.commands.last_mut().unwrap();
        loop {
            if let Some(status) = flight.poll(deadline)? {
                let output = flight.output()?;
                if !status.success() && !(args.first() == Some(&"show") && status.code() == Some(1))
                {
                    return Err(io::Error::other(format!(
                        "accepted unit command failed: {status}: {output}"
                    )));
                }
                return Ok(output);
            }
            if let Err(error) = pause(deadline) {
                let _ = flight.kill_group();
                return Err(error);
            }
        }
    }
    fn show(&mut self, unit: &str, deadline: Instant) -> io::Result<String> {
        self.command(false, &["show", "--no-pager", "--property=Id,LoadState,ActiveState,SubState,ControlGroup,InvocationID,MainPID,ControlPID,Result,ExecMainCode,ExecMainStatus", unit], deadline)
    }
    fn capture_helper(
        &mut self,
        identity: &UnitIdentity,
        deadline: Instant,
    ) -> io::Result<OwnedFd> {
        let first = self.show(&identity.unit, deadline)?;
        let p = process::properties(&first)?;
        if p.get("InvocationID") != Some(&identity.invocation.as_str()) {
            return Err(io::Error::other("accepted helper unit invocation changed"));
        }
        let pid: i32 = p
            .get("MainPID")
            .ok_or_else(|| io::Error::other("missing helper PID"))?
            .parse()
            .map_err(io::Error::other)?;
        if pid <= 0 {
            return Err(io::Error::other("accepted helper is not live at startup"));
        }
        let task_identity = || -> io::Result<(u64, String)> {
            let read = |name: &str| -> io::Result<String> {
                let mut text = String::new();
                File::open(format!("/proc/{pid}/{name}"))?
                    .take(8193)
                    .read_to_string(&mut text)?;
                if text.len() > 8192 {
                    return Err(io::Error::other("accepted task identity exceeded bound"));
                }
                Ok(text)
            };
            let stat = read("stat")?;
            let tail = stat
                .rsplit_once(')')
                .ok_or_else(|| io::Error::other("malformed helper stat"))?
                .1;
            let birth = tail
                .split_whitespace()
                .nth(19)
                .ok_or_else(|| io::Error::other("missing helper birth"))?
                .parse()
                .map_err(io::Error::other)?;
            Ok((birth, read("cgroup")?))
        };
        let before = task_identity()?;
        let expected_group = p
            .get("ControlGroup")
            .ok_or_else(|| io::Error::other("missing helper cgroup"))?;
        if before
            .1
            .lines()
            .filter_map(|line| line.strip_prefix("0::"))
            .collect::<Vec<_>>()
            != [*expected_group]
        {
            return Err(io::Error::other(
                "accepted helper does not belong to retained unit cgroup",
            ));
        }
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let pin = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        let second = self.show(&identity.unit, deadline)?;
        let q = process::properties(&second)?;
        if q.get("Id") != Some(&identity.unit.as_str())
            || q.get("InvocationID") != p.get("InvocationID")
            || q.get("MainPID") != p.get("MainPID")
            || q.get("ControlGroup") != p.get("ControlGroup")
            || task_identity()? != before
            || pidfd_terminal(pin.as_fd())?
        {
            return Err(io::Error::other(
                "accepted helper changed while retaining its exact PIDFD",
            ));
        }
        Ok(pin)
    }

    /// Called inside the actual parent startup callback, before STARTUP_READY.
    /// The service remains owned by the exchange until it returns; command and
    /// unit identity custody stays here even when observation fails.
    pub fn observe_startup(
        &mut self,
        owner: &crate::network_container::NetworkParentOwnership,
        deadline: Instant,
    ) -> io::Result<()> {
        use crate::network_container::NetworkParentOwnership;
        let service = match owner {
            NetworkParentOwnership::Running(service) => service,
            NetworkParentOwnership::StartupFailed(failure) => &failure.owner,
        };
        if self.loader.is_some() {
            return Err(io::Error::other("accepted startup observed twice"));
        }
        let unit = service.unit().to_owned();
        let identity = UnitIdentity::capture(&unit, &self.show(&unit, deadline)?)?;
        self.loader = Some(identity);
        let identity = self.loader.take().unwrap();
        let captured = self.capture_helper(&identity, deadline);
        self.loader = Some(identity);
        self.loader_task = Some(captured?);
        if let Err(error) = self.prepare_query(service, deadline) {
            // Do this before returning the first startup error to the enclosing
            // owner. The helper may still be waiting for its first SCM packet.
            if let Err(abort) = self.abort_query_channel() {
                self.query_abort_failure = Some(abort.to_string());
            }
            return Err(error);
        }
        Ok(())
    }
    fn prepare_query(
        &mut self,
        service: &ParentAcceptedService,
        deadline: Instant,
    ) -> io::Result<()> {
        if self.query.is_some() {
            return Err(io::Error::other("accepted query already prepared"));
        }
        let mut pair = [-1; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let endpoint = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let input = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        let unit = format!(
            "hermit-accepted-readback-{}.service",
            service
                .incarnation()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let bootstrap_before_ns = bootstrap_deadline_ns(deadline)?;
        let arguments = [
            "--accepted-readback-private-stdin-v1".into(),
            bootstrap_before_ns.to_string().into(),
        ];
        let launch = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::AcceptedReadback,
            unit: &unit,
            executable: &self.executable,
            arguments: &arguments,
            lifetime: CapabilityServiceLifetime::ControllerOwned,
            writable_directories: &[],
        };
        // CommandFlight retains its actual wrapper and bounded stdout/stderr.
        let mut command = Command::new(CAPABILITY_SUDO);
        command
            .env_clear()
            .envs(CAPABILITY_ENVIRONMENT.iter().copied())
            .args(launch.arguments()?);
        self.query = Some((
            unit.clone(),
            CommandFlight::start_with_stdin(&mut command, Stdio::from(input))?,
            None,
            endpoint,
        ));
        let identity = loop {
            let shown = self.show(&unit, deadline)?;
            match UnitIdentity::capture(&unit, &shown) {
                Ok(identity) => break identity,
                Err(error) => {
                    if self.query.as_mut().unwrap().1.poll(deadline)?.is_some() {
                        return Err(error);
                    }
                    pause(deadline)?;
                }
            }
        };
        self.query.as_mut().unwrap().2 = Some(identity);
        let identity = self.query.as_mut().unwrap().2.take().unwrap();
        let captured = self.capture_helper(&identity, deadline);
        self.query.as_mut().unwrap().2 = Some(identity);
        self.query_task = Some(captured?);
        send_bootstrap(
            self.query.as_ref().unwrap().3.as_fd(),
            service.controller_pidfd(),
            service.incarnation(),
        )?;
        self.query_bootstrapped = true;
        Ok(())
    }
    fn validate_query_custody(&self) -> io::Result<()> {
        if self.query_aborted {
            return Err(io::Error::other("accepted query startup was aborted"));
        }
        let query = self
            .query
            .as_ref()
            .ok_or_else(|| io::Error::other("accepted query command was not started"))?;
        let identity = query
            .2
            .as_ref()
            .ok_or_else(|| io::Error::other("accepted query unit identity was not captured"))?;
        if identity.unit != query.0 {
            return Err(io::Error::other("accepted query unit owner changed"));
        }
        self.query_task
            .as_ref()
            .ok_or_else(|| io::Error::other("accepted query PIDFD was not captured"))?;
        if !self.query_bootstrapped {
            return Err(io::Error::other(
                "accepted query controller bootstrap did not complete",
            ));
        }
        Ok(())
    }
    fn abort_query_channel(&mut self) -> io::Result<()> {
        let Some(query) = self.query.as_ref() else {
            return Ok(());
        };
        if self.query_aborted {
            return Ok(());
        }
        // Keep the original descriptor for custody/readback. Shutdown wakes a
        // live helper blocked before bootstrap and cannot mint a query proof.
        if unsafe { libc::shutdown(query.3.as_raw_fd(), libc::SHUT_RDWR) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.query_aborted = true;
        Ok(())
    }
    fn abort_query(&mut self, deadline: Instant) -> io::Result<()> {
        let mut error = self.abort_query_channel().err();
        if self.query.is_none() {
            return error.map_or(Ok(()), Err);
        }
        // Cleanup continues through the original known owners even when an
        // earlier preparation phase never supplied unit/PIDFD authority. No
        // lookup manufactures those missing owners after the first failure.
        if let Some(task) = self.query_task.as_ref() {
            let terminal = (|| {
                while !pidfd_terminal(task.as_fd())? {
                    pause(deadline)?;
                }
                within(deadline)
            })();
            if let Err(failure) = terminal {
                error.get_or_insert(failure);
            }
        } else {
            error.get_or_insert_with(|| {
                io::Error::other("aborted query lacks original helper PIDFD")
            });
        }
        let identity = self.query.as_mut().expect("retained query").2.take();
        if let Some(identity) = identity {
            let removed = if identity.unit == self.query.as_ref().expect("retained query").0 {
                self.remove_unit(&identity, AcceptedUnitRole::Query, deadline)
            } else {
                Err(io::Error::other("aborted query unit identity changed"))
            };
            self.query.as_mut().expect("retained query").2 = Some(identity);
            if let Err(failure) = removed {
                error.get_or_insert(failure);
            }
        } else {
            error.get_or_insert_with(|| {
                io::Error::other("aborted query lacks original unit identity")
            });
        }
        let flight = &mut self.query.as_mut().expect("retained query").1;
        let joined = (|| {
            loop {
                if flight.poll(deadline)?.is_some() {
                    return Ok(());
                }
                pause(deadline)?;
            }
        })();
        if let Err(failure) = joined {
            // This is still the original unreaped CommandFlight group; its
            // existing owner retains the wait. No new cleanup window follows.
            let _ = flight.kill_group();
            error.get_or_insert(failure);
        }
        error.map_or(Ok(()), Err)
    }
    fn join_unit_wrapper(
        &mut self,
        role: AcceptedUnitRole,
        deadline: Instant,
    ) -> io::Result<ExitStatus> {
        match role {
            AcceptedUnitRole::Loader => {
                let status = loop {
                    if let Some(status) = self
                        .service
                        .as_mut()
                        .ok_or_else(|| io::Error::other("accepted service was never started"))?
                        .poll_launcher_terminal()?
                    {
                        break status;
                    }
                    pause(deadline)?;
                };
                let group = self
                    .service()?
                    .launcher_group()
                    .ok_or_else(|| io::Error::other("accepted wrapper was never launched"))?;
                while !group_absent(group, deadline)? {
                    pause(deadline)?;
                }
                within(deadline)?;
                Ok(status)
            }
            AcceptedUnitRole::Query => {
                let flight = &mut self
                    .query
                    .as_mut()
                    .ok_or_else(|| io::Error::other("accepted query owner missing"))?
                    .1;
                loop {
                    if let Some(status) = flight.poll(deadline)? {
                        return Ok(status);
                    }
                    pause(deadline)?;
                }
            }
        }
    }
    fn reset_failed_unit(
        &mut self,
        identity: &UnitIdentity,
        role: AcceptedUnitRole,
        status: ExitStatus,
        shown: &str,
        deadline: Instant,
    ) -> io::Result<bool> {
        within(deadline)?;
        let (unit, task, group) = match role {
            AcceptedUnitRole::Loader => (
                self.service()?.unit(),
                self.loader_task.as_ref(),
                self.service()?.launcher_group(),
            ),
            AcceptedUnitRole::Query => {
                let query = self
                    .query
                    .as_ref()
                    .ok_or_else(|| io::Error::other("accepted query owner missing"))?;
                (
                    query.0.as_str(),
                    self.query_task.as_ref(),
                    Some(query.1.child.id()),
                )
            }
        };
        if unit != identity.unit {
            return Err(io::Error::other("failed accepted unit identity changed"));
        }
        if let Some(task) = task {
            if !pidfd_terminal(task.as_fd())? {
                return Err(io::Error::other("failed accepted helper remains live"));
            }
        }
        if !group_absent(
            group.ok_or_else(|| io::Error::other("accepted wrapper group missing"))?,
            deadline,
        )? {
            return Err(io::Error::other(
                "failed accepted unit wrapper group remains live",
            ));
        }
        let p = process::properties(shown)?;
        if p.get("Id") != Some(&identity.unit.as_str())
            || p.get("InvocationID") != Some(&identity.invocation.as_str())
            || p.get("LoadState") != Some(&"loaded")
            || p.get("ActiveState") != Some(&"failed")
            || p.get("SubState") != Some(&"failed")
            || p.get("MainPID") != Some(&"0")
            || p.get("ControlPID") != Some(&"0")
        {
            return Err(io::Error::other(
                "failed accepted unit identity/terminal state changed",
            ));
        }
        let result = p
            .get("Result")
            .filter(|v| !v.is_empty() && **v != "success")
            .ok_or_else(|| {
                io::Error::other("failed accepted unit lacks original failure result")
            })?;
        let code: i32 = p
            .get("ExecMainCode")
            .ok_or_else(|| io::Error::other("failed accepted unit lacks exit code"))?
            .parse()
            .map_err(io::Error::other)?;
        let exit: i32 = p
            .get("ExecMainStatus")
            .ok_or_else(|| io::Error::other("failed accepted unit lacks exit status"))?
            .parse()
            .map_err(io::Error::other)?;
        // A later unlink/unload cannot erase an already observed failure,
        // including one seen while the original cgroup was still present.
        self.first_failed_manager_readbacks
            .entry(identity.unit.clone())
            .or_insert_with(|| shown.to_owned());
        let held = identity.directory.metadata()?;
        if held.dev() != identity.device || held.ino() != identity.inode {
            return Err(io::Error::other(
                "failed accepted unit held cgroup identity changed",
            ));
        }
        match identity.cgroup.symlink_metadata() {
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
            Err(error) => return Err(error),
            Ok(named) if named.dev() != identity.device || named.ino() != identity.inode => {
                return Err(io::Error::other("failed accepted unit cgroup was replaced"));
            }
            Ok(_) => return Ok(false),
        }
        let after = identity.directory.metadata()?;
        if after.dev() != identity.device || after.ino() != identity.inode {
            return Err(io::Error::other(
                "failed accepted unit held cgroup identity changed",
            ));
        }
        if held.nlink() != 0 || after.nlink() != 0 || p.get("ControlGroup") != Some(&"") {
            return Ok(false);
        }
        within(deadline)?;
        if self.failed_unit_observations.len() >= 2
            || !self.failed_reset_intents.insert(identity.unit.clone())
        {
            return Err(io::Error::other(
                "accepted failed unit reset intent cannot repeat",
            ));
        }
        // This is failure-retirement evidence only. Preserve the original failed
        // manager record and actual wrapper wait before reset-failed clears it.
        // The existing runner owns and bounds stderr; no successful receipt's
        // strict parser or complete-inventory requirement is changed.
        self.failed_unit_observations.push(serde_json::json!({
            "schema":"hermit-accepted-failed-unit-before-reset-v1", "unit":identity.receipt(),
            "manager_readback":shown, "result":result, "exec_main_code":code,
            "first_failed_manager_readback":self.first_failed_manager_readbacks.get(&identity.unit),
            "exec_main_status":exit, "wrapper_wait":status.into_raw(),
            "helper_pidfd_captured":task.is_some(),
            "held_nlink_before":held.nlink(), "held_nlink_after":after.nlink(),
            "observed_ns":now_ns()?, "prior_command_count":self.commands.len()
        }));
        let mut bytes = serde_json::to_vec(self.failed_unit_observations.last().unwrap())?;
        bytes.push(b'\n');
        if bytes.len() > 16_384 {
            return Err(io::Error::other(
                "accepted failed unit observation exceeded bound",
            ));
        }
        let mut stderr = io::stderr().lock();
        stderr.write_all(&bytes)?;
        stderr.flush()?;
        within(deadline)?;
        self.command(
            true,
            &["--no-ask-password", "reset-failed", &identity.unit],
            deadline,
        )?;
        within(deadline)?;
        Ok(true)
    }
    fn remove_unit(
        &mut self,
        identity: &UnitIdentity,
        role: AcceptedUnitRole,
        deadline: Instant,
    ) -> io::Result<(ExitStatus, bool)> {
        // Accepted helpers are not RemainAfterExit units: their actual helper
        // terminal makes the wrapper finish and lets systemd retire successful
        // cgroups without a separate stop round trip. Failed records still need
        // an explicitly proven, scoped reset. Keep one stop fallback for loaded
        // nonfailed partial/older launch state under the same deadline.
        let status = self.join_unit_wrapper(role, deadline)?;
        let mut stopped = false;
        loop {
            let shown = self.show(&identity.unit, deadline)?;
            if identity.drained(&shown)? {
                within(deadline)?;
                return Ok((
                    status,
                    self.first_failed_manager_readbacks
                        .contains_key(&identity.unit),
                ));
            }
            if process::properties(&shown)?.get("ActiveState") == Some(&"failed") {
                self.reset_failed_unit(identity, role, status, &shown, deadline)?;
            } else if !stopped {
                self.command(
                    true,
                    &["--no-ask-password", "stop", &identity.unit],
                    deadline,
                )?;
                stopped = true;
            }
            pause(deadline)?;
        }
    }
    /// Exactly once, under the original aggregate cleanup deadline. Every BPF
    /// scan also uses the stricter service close+1s boundary from its raw report.
    pub fn drain(&mut self, deadline: Instant) -> io::Result<Value> {
        if self.deadline.is_some() {
            return Err(io::Error::other(
                "accepted finalization cannot restart its deadline",
            ));
        }
        self.deadline = Some(deadline);
        let result = self.drain_once(deadline);
        if let Err(error) = &result {
            self.failure = Some(error.to_string());
            if let Err(abort) = self.abort_query(self.deadline.unwrap_or(deadline).min(deadline)) {
                self.query_abort_failure = Some(abort.to_string());
            }
        }
        // An abort may reap its exact owners, but cannot change the original
        // failure into an object-absence certificate or publish guest success.
        result
    }
    fn drain_once(&mut self, deadline: Instant) -> io::Result<Value> {
        within(deadline)?;
        if !self.service()?.controller_has_exited()? {
            return Err(io::Error::other("actual accepted controller remains live"));
        }
        if self.loader_task.is_some() {
            loop {
                // The service's close protocol waits for the CLI's original Child
                // joins. Drive that same retained owner before waiting for service exit.
                self.progress_grouped(deadline)?;
                if pidfd_terminal(self.loader_task.as_ref().unwrap().as_fd())? {
                    break;
                }
                pause(deadline)?;
            }
        }
        // Preserve all raw partial-load/failure records even if READY was never
        // delivered. They cannot become successful empty-inventory evidence.
        let stdout = log(&mut self.stdout)?;
        let stderr = log(&mut self.stderr)?;
        let loader = self.loader.as_ref().ok_or_else(|| {
            io::Error::other("accepted loader identity was never captured before STARTUP_READY")
        })?;
        if loader.unit != self.service()?.unit() {
            return Err(io::Error::other("accepted startup unit owner changed"));
        }
        let loader = self.loader.take().unwrap();
        let removed = self.remove_unit(&loader, AcceptedUnitRole::Loader, deadline);
        self.loader = Some(loader);
        let (status, loader_failed) = removed?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "accepted service wrapper failed {status}; raw logs retained"
            )));
        }
        if loader_failed {
            return Err(io::Error::other(
                "accepted loader failed manager result remains retained",
            ));
        }
        // Startup may have failed after retaining only part of the query
        // owner. Keep that custody and its first error; do not send, reconstruct
        // or unwrap an incomplete helper after finishing the original loader.
        self.validate_query_custody()?;
        let ids = self.service()?.original_ids().ok_or_else(|| {
            io::Error::other("accepted startup had no authenticated READY inventory")
        })?;
        let ProviderArtifact {
            maps,
            programs,
            links,
            ..
        } = self.service()?.expected_artifact();
        let counts = [*maps, *programs, *links];
        validate_ids(&ids, counts)?;
        let run = self.service()?.incarnation();
        let closed_ns = closed_inventory(&stdout, run, &ids)?;
        let query = Query {
            run,
            ids: ids.clone(),
            counts,
            closed_ns,
            deadline_ns: closed_ns
                .checked_add(CLOSE_NS)
                .ok_or_else(|| io::Error::other("accepted deadline overflow"))?,
        };
        query.validate()?;
        let remaining = query.deadline_ns.checked_sub(now_ns()?).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "accepted close deadline already elapsed",
            )
        })?;
        let query_deadline =
            (Instant::now() + std::time::Duration::from_nanos(remaining)).min(deadline);
        // Any failure after original-ID querying starts keeps this earlier cut
        // through abort; the aggregate deadline cannot extend close+1s.
        self.deadline = Some(self.deadline.unwrap_or(deadline).min(query_deadline));
        let bytes = serde_json::to_vec(&query)?;
        within(query_deadline)?;
        let sent = unsafe {
            libc::send(
                self.query
                    .as_ref()
                    .ok_or_else(|| io::Error::other("accepted query owner missing"))?
                    .3
                    .as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        if sent != bytes.len() as isize {
            return Err(io::Error::other(
                "accepted original-ID request was not sent exactly once",
            ));
        }
        while !pidfd_terminal(
            self.query_task
                .as_ref()
                .ok_or_else(|| io::Error::other("accepted query PIDFD was not captured"))?
                .as_fd(),
        )? {
            pause(query_deadline)?;
        }
        let identity = self
            .query
            .as_mut()
            .ok_or_else(|| io::Error::other("accepted query owner missing"))?
            .2
            .take()
            .ok_or_else(|| io::Error::other("accepted query identity missing"))?;
        let removed = self.remove_unit(&identity, AcceptedUnitRole::Query, query_deadline);
        self.query
            .as_mut()
            .ok_or_else(|| io::Error::other("accepted query owner missing"))?
            .2 = Some(identity);
        let (query_status, query_failed) = removed?;
        let reply = self
            .query
            .as_mut()
            .ok_or_else(|| io::Error::other("accepted query owner missing"))?
            .1
            .output()?;
        if !query_status.success() {
            return Err(io::Error::other(format!(
                "accepted original-ID query failed {query_status}"
            )));
        }
        if query_failed {
            return Err(io::Error::other(
                "accepted query failed manager result remains retained",
            ));
        }
        let readback: Readback = serde_json::from_str(&reply)?;
        readback.validate(&query)?;
        self.require_grouped_completed()?;
        let observed_ns = now_ns()?;
        if observed_ns >= query.deadline_ns {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "accepted actors drained after original close deadline",
            ));
        }
        Ok(
            serde_json::json!({"schema":"hermit-accepted-parent-terminal-v1", "run":run,
            "original_ids":ids, "counts":counts, "wrapper_wait":status.into_raw(),
            "loader":self.loader.as_ref().unwrap().receipt(), "query_wait":query_status.into_raw(),
            "query":self.query.as_ref().ok_or_else(|| io::Error::other("accepted query owner missing"))?.2.as_ref().ok_or_else(|| io::Error::other("accepted query identity missing"))?.receipt(),
            "readback":readback, "terminal_observed_ns":observed_ns,
            "service_stdout":stdout, "service_stderr":stderr}),
        )
    }
}

/// Actual accepted receipt files and their private root, retained before I/O.
/// Serialized observations never substitute for the live finalizer's custody.
pub struct AcceptedRecovery {
    grouped_root: Option<crate::unix_guard_package::RecoveryDeploymentRoot>,
    grouped_parent: Option<File>,
    grouped_attempted: bool,
    root: crate::unix_guard_package::RecoveryDeploymentRoot,
    label: String,
    artifact: Option<ProviderArtifact>,
    files: [Option<File>; 3],
    identities: Option<[ReceiptFileIdentity; 3]>,
    started: Option<StartupReceipt>,
    attempted: bool,
    failed: bool,
    finished: bool,
}
/// A failed launch can retain one accepted provider, one readback unit and a
/// grouped sibling. Bound those unresolved populations across fresh CLI
/// processes; completed receipts do not consume this budget.
const MAX_UNRESOLVED_ACCEPTED_LAUNCHES: usize = 8;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptFileIdentity {
    device: u64,
    inode: u64,
    uid: u32,
    mode: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActorReceipt {
    unit: String,
    invocation: String,
    cgroup: PathBuf,
    device: u64,
    inode: u64,
}
fn receipt_artifact<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<ProviderArtifact, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Strict {
        topology: detcore::network_runtime::ProviderTopology,
        wire_format: detcore::network_runtime::ProviderWireFormat,
        object_sha256: [u8; 32],
        library_sha256: [u8; 32],
        btf_sha256: [u8; 32],
        maps: usize,
        programs: usize,
        links: usize,
    }
    let value = Strict::deserialize(deserializer)?;
    Ok(ProviderArtifact {
        topology: value.topology,
        wire_format: value.wire_format,
        object_sha256: value.object_sha256,
        library_sha256: value.library_sha256,
        btf_sha256: value.btf_sha256,
        maps: value.maps,
        programs: value.programs,
        links: value.links,
    })
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupReceipt {
    schema: String,
    run: [u8; 16],
    #[serde(deserialize_with = "receipt_artifact")]
    artifact: ProviderArtifact,
    loader: ActorReceipt,
    query: ActorReceipt,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalReceipt {
    schema: String,
    run: [u8; 16],
    original_ids: Vec<(u32, u32)>,
    counts: [usize; 3],
    wrapper_wait: i32,
    loader: ActorReceipt,
    query_wait: i32,
    query: ActorReceipt,
    readback: Readback,
    terminal_observed_ns: u64,
    service_stdout: String,
    service_stderr: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogReadback {
    identity: ReceiptFileIdentity,
    bytes: usize,
    sha256: [u8; 32],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BeforeReceipt {
    schema: u32,
    stage: String,
    label: String,
    root: crate::unix_guard_package::RecoveryDirectoryIdentity,
    #[serde(deserialize_with = "receipt_artifact")]
    artifact: ProviderArtifact,
    files: [ReceiptFileIdentity; 3],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartedReceipt {
    schema: u32,
    stage: String,
    label: String,
    observed: StartupReceipt,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletedReceipt {
    schema: u32,
    stage: String,
    label: String,
    root: crate::unix_guard_package::RecoveryDirectoryIdentity,
    stdout: LogReadback,
    stderr: LogReadback,
    observed: TerminalReceipt,
}
fn receipt_label(label: &str) -> io::Result<()> {
    if label.len() != 32
        || !label
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || label.bytes().all(|b| b == b'0')
    {
        return Err(io::Error::other("accepted receipt label differs"));
    }
    Ok(())
}
fn receipt_names(label: &str) -> [String; 3] {
    [
        format!("accepted-{label}.terminal.jsonl"),
        format!("accepted-{label}.stdout.log"),
        format!("accepted-{label}.stderr.log"),
    ]
}
fn receipt_identity(file: &File) -> io::Result<ReceiptFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    if !m.is_file()
        || m.uid() != unsafe { libc::getuid() }
        || m.mode() & 0o7777 != 0o600
        || m.nlink() != 1
        || m.len() > LOG_BYTES
    {
        return Err(io::Error::other(
            "accepted receipt file shape/owner/extent differs",
        ));
    }
    Ok(ReceiptFileIdentity {
        device: m.dev(),
        inode: m.ino(),
        uid: m.uid(),
        mode: m.mode(),
    })
}
fn receipt_open_owned(root: BorrowedFd<'_>, name: &str, create: bool) -> io::Result<File> {
    if name.contains('/') || name.contains('\0') {
        return Err(io::Error::other("accepted receipt basename differs"));
    }
    let name = std::ffi::CString::new(name).map_err(io::Error::other)?;
    let flags = libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if create {
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL
        } else {
            libc::O_RDONLY
        };
    let raw = unsafe { libc::openat(root.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(raw) })
}
fn receipt_open(root: BorrowedFd<'_>, name: &str, create: bool) -> io::Result<File> {
    let file = receipt_open_owned(root, name, create)?;
    receipt_identity(&file)?;
    Ok(file)
}
fn receipt_snapshot(file: &File) -> io::Result<(ReceiptFileIdentity, u64, i64, i64, i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let identity = receipt_identity(file)?;
    let m = file.metadata()?;
    if m.len() > LOG_BYTES {
        return Err(io::Error::other("accepted receipt grew beyond bound"));
    }
    Ok((
        identity,
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    ))
}
fn receipt_read(file: &File) -> io::Result<Vec<u8>> {
    let before = receipt_snapshot(file)?;
    let length = usize::try_from(before.1).map_err(io::Error::other)?;
    let mut bytes = vec![0; length];
    let mut offset = 0;
    while offset < length {
        let n = unsafe {
            libc::pread(
                file.as_raw_fd(),
                bytes[offset..].as_mut_ptr().cast(),
                length - offset,
                offset as libc::off_t,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::Error::other("accepted receipt was truncated"));
        }
        offset += n as usize;
    }
    if receipt_snapshot(file)? != before {
        return Err(io::Error::other("accepted receipt changed during readback"));
    }
    Ok(bytes)
}
fn actor_receipt(actor: &ActorReceipt, expected_prefix: &str) -> io::Result<()> {
    let suffix = actor
        .unit
        .strip_prefix(expected_prefix)
        .and_then(|s| s.strip_suffix(".service"))
        .ok_or_else(|| io::Error::other("accepted actor role changed"))?;
    receipt_label(suffix)?;
    receipt_label(&actor.invocation)?;
    if actor.device == 0
        || actor.inode == 0
        || !actor.cgroup.starts_with("/sys/fs/cgroup")
        || !actor.cgroup.ends_with(&actor.unit)
        || actor.cgroup.components().any(|c| {
            matches!(
                c,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(io::Error::other("accepted actor cgroup identity differs"));
    }
    Ok(())
}
fn validate_startup(started: &StartupReceipt, artifact: &ProviderArtifact) -> io::Result<()> {
    artifact.topology.validate()?;
    let expected_counts = match artifact.topology {
        detcore::network_runtime::ProviderTopology::ClassicV40 => [23, 46, 58],
        detcore::network_runtime::ProviderTopology::GroupedV1 { .. } => [23, 44, 44],
        detcore::network_runtime::ProviderTopology::FtraceV1 { .. } => [23, 47, 47],
    };
    if [artifact.maps, artifact.programs, artifact.links] != expected_counts
        || artifact.object_sha256 == [0; 32]
        || artifact.library_sha256 == [0; 32]
        || artifact.btf_sha256 == [0; 32]
    {
        return Err(io::Error::other(
            "accepted receipt artifact population or identity differs",
        ));
    }
    let run_text = started
        .run
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if started.loader.unit != format!("hermit-accepted-{run_text}.service")
        || started.query.unit != format!("hermit-accepted-readback-{run_text}.service")
    {
        return Err(io::Error::other(
            "accepted actor units differ from actual startup run",
        ));
    }

    if started.schema != "hermit-accepted-parent-startup-v1"
        || started.run == [0; 16]
        || &started.artifact != artifact
    {
        return Err(io::Error::other("accepted startup run/artifact changed"));
    }
    actor_receipt(&started.loader, "hermit-accepted-")?;
    actor_receipt(&started.query, "hermit-accepted-readback-")?;
    if started.loader.unit == started.query.unit
        || started.loader.invocation == started.query.invocation
        || (started.loader.device, started.loader.inode)
            == (started.query.device, started.query.inode)
    {
        return Err(io::Error::other("accepted actor identities alias"));
    }
    Ok(())
}
// Reject duplicate JSON keys before Value can collapse them. The live
// finalizer's original closed_inventory predicates remain unchanged below.
struct UniqueReceiptJson(Value);
impl<'de> Deserialize<'de> for UniqueReceiptJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueReceiptJson;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON with unique keys")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(UniqueReceiptJson(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(UniqueReceiptJson(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(UniqueReceiptJson(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| UniqueReceiptJson(Value::Number(n)))
                    .ok_or_else(|| E::custom("nonfinite JSON number"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(UniqueReceiptJson(v.into()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(UniqueReceiptJson(v.into()))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueReceiptJson(Value::Null))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueReceiptJson(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueReceiptJson(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(UniqueReceiptJson(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate accepted receipt key {key}"
                        )));
                    }
                    let UniqueReceiptJson(value) = map.next_value()?;
                    values.insert(key, value);
                }
                Ok(UniqueReceiptJson(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
fn strict_service_transcript(text: &str) -> io::Result<()> {
    if !text.ends_with('\n') {
        return Err(io::Error::other("accepted service transcript is partial"));
    }
    let rows = text
        .lines()
        .map(serde_json::from_str::<UniqueReceiptJson>)
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() != 2 {
        return Err(io::Error::other(
            "accepted service transcript phase census differs",
        ));
    }
    let before = &rows[0].0;
    let after = &rows[1].0;
    let explicit_null = |row: &Value, key: &str| row.get(key).is_some_and(Value::is_null);
    if !explicit_null(before, "failure") || !explicit_null(after, "close_error") {
        return Err(io::Error::other(
            "accepted service transcript lacks explicit clean error fields",
        ));
    }
    for inventory in before
        .get("inventories")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("accepted service transcript lacks inventories"))?
    {
        if !explicit_null(&inventory["status"], "errno") {
            return Err(io::Error::other(
                "accepted inventory lacks explicit null errno",
            ));
        }
    }
    for receipt in after
        .get("close_receipts")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("accepted service transcript lacks close receipts"))?
    {
        if !explicit_null(&receipt["close"], "errno")
            || !explicit_null(&receipt["inventory"]["status"], "errno")
        {
            return Err(io::Error::other("accepted close lacks explicit null errno"));
        }
    }
    Ok(())
}

fn validate_terminal(
    started: &StartupReceipt,
    terminal: &TerminalReceipt,
    stdout: &[u8],
    stderr: &[u8],
) -> io::Result<()> {
    validate_startup(started, &started.artifact)?;
    let counts = [
        started.artifact.maps,
        started.artifact.programs,
        started.artifact.links,
    ];
    if terminal.schema != "hermit-accepted-parent-terminal-v1"
        || terminal.run != started.run
        || terminal.counts != counts
        || terminal.wrapper_wait != 0
        || terminal.query_wait != 0
        || terminal.loader != started.loader
        || terminal.query != started.query
        || terminal.service_stdout.as_bytes() != stdout
        || terminal.service_stderr.as_bytes() != stderr
    {
        return Err(io::Error::other(
            "accepted terminal changed original startup/log/actor identity",
        ));
    }
    validate_ids(&terminal.original_ids, counts)?;
    strict_service_transcript(&terminal.service_stdout)?;
    let closed = closed_inventory(
        &terminal.service_stdout,
        started.run,
        &terminal.original_ids,
    )?;
    let query = Query {
        run: started.run,
        ids: terminal.original_ids.clone(),
        counts,
        closed_ns: closed,
        deadline_ns: closed
            .checked_add(CLOSE_NS)
            .ok_or_else(|| io::Error::other("accepted receipt close deadline overflow"))?,
    };
    terminal.readback.validate(&query)?;
    if terminal.terminal_observed_ns < terminal.readback.passes_ns[1]
        || terminal.terminal_observed_ns >= query.deadline_ns
    {
        return Err(io::Error::other(
            "accepted terminal receipt exceeds original deadline",
        ));
    }
    Ok(())
}
impl AcceptedRecovery {
    /// Move the actual root into a recovery scope before any fallible creation.
    pub fn retain(root: crate::unix_guard_package::RecoveryDeploymentRoot, label: String) -> Self {
        Self {
            grouped_root: None,
            grouped_parent: None,
            grouped_attempted: false,
            root,
            label,
            artifact: None,
            files: [None, None, None],
            identities: None,
            started: None,
            attempted: false,
            failed: false,
            finished: false,
        }
    }
    /// Create exactly three retained files relative to the held directory.
    pub fn initialize(&mut self, artifact: ProviderArtifact) -> io::Result<()> {
        if self.attempted {
            return Err(io::Error::other(
                "accepted receipt initialization cannot restart",
            ));
        }
        self.attempted = true;
        let result = self.initialize_once(artifact);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn initialize_once(&mut self, artifact: ProviderArtifact) -> io::Result<()> {
        receipt_label(&self.label)?;
        self.root.identity()?;
        admit_accepted_launch(&self.root)?;
        self.artifact = Some(artifact);
        for (i, name) in receipt_names(&self.label).iter().enumerate() {
            // Retain each actual description before validating metadata or
            // creating the next file. Failed initialization keeps partial custody.
            self.files[i] = Some(receipt_open_owned(self.root.directory.as_fd(), name, true)?);
            receipt_identity(self.files[i].as_ref().unwrap())?;
        }
        let ids = [
            receipt_identity(self.files[0].as_ref().unwrap())?,
            receipt_identity(self.files[1].as_ref().unwrap())?,
            receipt_identity(self.files[2].as_ref().unwrap())?,
        ];
        if ids
            .iter()
            .map(|v| (v.device, v.inode))
            .collect::<BTreeSet<_>>()
            .len()
            != 3
        {
            return Err(io::Error::other("accepted receipt files alias"));
        }
        self.identities = Some(ids.clone());
        self.append(serde_json::to_value(BeforeReceipt {
            schema: 1,
            stage: "accepted_before_launch".into(),
            label: self.label.clone(),
            root: self.root.identity()?,
            artifact: self.artifact.clone().unwrap(),
            files: ids,
        })?)
    }
    fn check_names(&self) -> io::Result<()> {
        self.root.identity()?;
        for (i, name) in receipt_names(&self.label).iter().enumerate() {
            if let Some(file) = self.files[i].as_ref() {
                let held = receipt_identity(file)?;
                let named = receipt_open(self.root.directory.as_fd(), name, false)?;
                if receipt_identity(&named)? != held
                    || self.identities.as_ref().is_some_and(|ids| held != ids[i])
                {
                    return Err(io::Error::other(
                        "accepted receipt name/held identity changed",
                    ));
                }
            }
        }
        Ok(())
    }
    fn append(&mut self, value: Value) -> io::Result<()> {
        self.check_names()?;
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        let file = self.files[0]
            .as_mut()
            .ok_or_else(|| io::Error::other("accepted receipt was not created"))?;
        if file
            .metadata()?
            .len()
            .checked_add(bytes.len() as u64)
            .is_none_or(|n| n > LOG_BYTES)
        {
            return Err(io::Error::other(
                "accepted receipt exceeded original log bound",
            ));
        }
        file.seek(SeekFrom::End(0))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        if unsafe { libc::fsync(self.root.directory.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.check_names()
    }
    /// Preserve a startup/finalization failure in the same retained root.
    pub fn failure(&mut self, error: &str) -> io::Result<()> {
        self.failed = true;
        self.append(serde_json::json!({"schema":1,"stage":"accepted_failed","label":self.label,"error":error}))
    }
    /// Separate bounded broker journals leave the accepted root's exact
    /// three-file census unchanged. No existing directory may be reused.
    pub fn create_grouped_sibling(&mut self) -> io::Result<OwnedFd> {
        if self.grouped_attempted || self.failed || self.identities.is_none() {
            return Err(io::Error::other(
                "grouped journal root is repeated or accepted setup failed",
            ));
        }
        self.grouped_attempted = true;
        self.root.identity()?;
        receipt_label(&self.label)?;
        let parent_path = self
            .root
            .writable_path
            .parent()
            .ok_or_else(|| io::Error::other("accepted root has no parent"))?
            .to_owned();
        if parent_path.canonicalize()? != parent_path {
            return Err(io::Error::other("accepted root parent is not canonical"));
        }
        self.grouped_parent = Some(
            File::options()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&parent_path)?,
        );
        let parent = self.grouped_parent.as_ref().unwrap();
        let held = parent.metadata()?;
        let named = parent_path.symlink_metadata()?;
        if !held.is_dir()
            || held.uid() != unsafe { libc::getuid() }
            || held.mode() & 0o022 != 0
            || held.dev() != named.dev()
            || held.ino() != named.ino()
            || named.file_type().is_symlink()
        {
            return Err(io::Error::other(
                "accepted root parent identity or ownership differs",
            ));
        }
        let basename = format!("network-grouped-{}", self.label);
        let name = std::ffi::CString::new(basename.as_str()).map_err(io::Error::other)?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let raw = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        self.grouped_root = Some(crate::unix_guard_package::RecoveryDeploymentRoot {
            directory: unsafe { OwnedFd::from_raw_fd(raw) },
            writable_path: parent_path.join(basename),
        });
        let root = self.grouped_root.as_ref().unwrap();
        root.identity()?;
        self.root
            .require_disjoint(root.directory.as_fd(), &root.writable_path)?;
        self.root.identity()?;
        let named = parent_path.symlink_metadata()?;
        if held.dev() != named.dev() || held.ino() != named.ino() || named.file_type().is_symlink()
        {
            return Err(io::Error::other(
                "accepted root parent changed during broker creation",
            ));
        }
        if unsafe { libc::fsync(parent.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        root.directory.try_clone()
    }
    /// Duplicate the original readable/writable service-log descriptions.
    pub fn service_logs(&self) -> io::Result<(File, File)> {
        if self.failed || self.identities.is_none() {
            return Err(io::Error::other(
                "accepted recovery initialization is incomplete or failed",
            ));
        }
        self.check_names()?;
        Ok((
            self.files[1]
                .as_ref()
                .ok_or_else(|| io::Error::other("accepted stdout absent"))?
                .try_clone()?,
            self.files[2]
                .as_ref()
                .ok_or_else(|| io::Error::other("accepted stderr absent"))?
                .try_clone()?,
        ))
    }
    /// Bind the actual live finalizer observation before STARTUP_READY.
    pub fn started(&mut self, value: Value) -> io::Result<()> {
        if self.failed || self.identities.is_none() {
            return Err(io::Error::other(
                "accepted recovery initialization is incomplete or failed",
            ));
        }
        let result = self.started_once(value);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn started_once(&mut self, value: Value) -> io::Result<()> {
        if self.started.is_some() {
            return Err(io::Error::other("accepted startup receipt repeated"));
        }
        let observed: StartupReceipt = serde_json::from_value(value)?;
        validate_startup(
            &observed,
            self.artifact
                .as_ref()
                .ok_or_else(|| io::Error::other("accepted artifact absent"))?,
        )?;
        self.started = Some(observed.clone());
        self.append(serde_json::to_value(StartedReceipt {
            schema: 1,
            stage: "accepted_started".into(),
            label: self.label.clone(),
            observed,
        })?)
    }
    /// Publish receipt data only after the live owner completed every terminal
    /// gate, then verify the retained files and root again before returning.
    pub fn finish(&mut self, value: Value) -> io::Result<()> {
        if self.finished {
            return Err(io::Error::other("accepted terminal receipt repeated"));
        }
        self.finished = true;
        if self.failed || self.identities.is_none() || self.started.is_none() {
            return Err(io::Error::other(
                "accepted terminal lacks complete original startup",
            ));
        }
        self.check_names()?;
        let stdout = receipt_read(self.files[1].as_ref().unwrap())?;
        let stderr = receipt_read(self.files[2].as_ref().unwrap())?;
        let observed: TerminalReceipt = serde_json::from_value(value)?;
        validate_terminal(
            self.started
                .as_ref()
                .ok_or_else(|| io::Error::other("accepted terminal lacks original startup"))?,
            &observed,
            &stdout,
            &stderr,
        )?;
        let ids = self.identities.as_ref().unwrap();
        let log = |i: usize, bytes: &[u8]| LogReadback {
            identity: ids[i].clone(),
            bytes: bytes.len(),
            sha256: *detcore::Digest::new(bytes),
        };
        self.append(serde_json::to_value(CompletedReceipt {
            schema: 1,
            stage: "accepted_terminal".into(),
            label: self.label.clone(),
            root: self.root.identity()?,
            stdout: log(1, &stdout),
            stderr: log(2, &stderr),
            observed,
        })?)?;
        validate_accepted_receipt(&self.root, &self.label, self.artifact.as_ref().unwrap())
            .map(|_| ())
    }
}
/// Strict readback summary. This is evidence read from files, never a native
/// provider capability or a substitute for the actual finalizer's checks.
#[derive(Debug)]
pub struct AcceptedReceiptReadback {
    /// Exact source run shared by startup, close and both absence scans.
    pub run: [u8; 16],
    /// Full authenticated artifact inventory.
    pub counts: [usize; 3],
    /// Every original typed BPF identifier.
    pub original_ids: Vec<(u32, u32)>,
}
/// Independently read one exact three-file accepted population through the
/// authenticated root. Failure, partial, duplicate and extra rows refuse.
pub fn validate_accepted_receipt(
    root: &crate::unix_guard_package::RecoveryDeploymentRoot,
    label: &str,
    expected: &ProviderArtifact,
) -> io::Result<AcceptedReceiptReadback> {
    receipt_label(label)?;
    let identity = root.identity()?;
    let names = receipt_names(label);
    let files = names
        .iter()
        .map(|name| receipt_open(root.directory.as_fd(), name, false))
        .collect::<io::Result<Vec<_>>>()?;
    let bytes = files
        .iter()
        .map(receipt_read)
        .collect::<io::Result<Vec<_>>>()?;
    let text = std::str::from_utf8(&bytes[0]).map_err(io::Error::other)?;
    let rows: Vec<_> = text.lines().collect();
    if rows.len() != 3 || !text.ends_with('\n') {
        return Err(io::Error::other(
            "accepted receipt incomplete/failed/extra row",
        ));
    }
    let before: BeforeReceipt = serde_json::from_str(rows[0])?;
    let started: StartedReceipt = serde_json::from_str(rows[1])?;
    let terminal: CompletedReceipt = serde_json::from_str(rows[2])?;
    if before.schema != 1
        || before.stage != "accepted_before_launch"
        || started.schema != 1
        || started.stage != "accepted_started"
        || terminal.schema != 1
        || terminal.stage != "accepted_terminal"
        || before.label != label
        || started.label != label
        || terminal.label != label
        || before.root != identity
        || terminal.root != identity
    {
        return Err(io::Error::other(
            "accepted receipt root/label/phase changed",
        ));
    }
    if &before.artifact != expected {
        return Err(io::Error::other(
            "accepted receipt differs from independently authenticated package",
        ));
    }
    validate_startup(&started.observed, expected)?;
    validate_terminal(&started.observed, &terminal.observed, &bytes[1], &bytes[2])?;
    for i in 0..3 {
        if before.files[i] != receipt_identity(&files[i])? {
            return Err(io::Error::other(
                "accepted file differs from original launch identity",
            ));
        }
    }
    for (index, proof) in [(1, &terminal.stdout), (2, &terminal.stderr)] {
        if proof.identity != before.files[index]
            || proof.bytes != bytes[index].len()
            || proof.sha256 != *detcore::Digest::new(&bytes[index])
        {
            return Err(io::Error::other("accepted retained log bytes/hash differ"));
        }
    }
    for actor in [&terminal.observed.loader, &terminal.observed.query] {
        match actor.cgroup.symlink_metadata() {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
            Ok(_) => {
                return Err(io::Error::other(
                    "accepted terminal actor cgroup still present",
                ));
            }
        }
    }
    // Verify held/name identities after all parsing as well as before reading.
    for (i, name) in names.iter().enumerate() {
        if receipt_identity(&receipt_open(root.directory.as_fd(), name, false)?)? != before.files[i]
        {
            return Err(io::Error::other(
                "accepted receipt name replaced during readback",
            ));
        }
    }
    for (file, original) in files.iter().zip(&bytes) {
        if receipt_read(file)? != *original {
            return Err(io::Error::other(
                "accepted receipt bytes changed during validation",
            ));
        }
    }
    if root.identity()? != identity {
        return Err(io::Error::other("accepted root replaced during readback"));
    }
    Ok(AcceptedReceiptReadback {
        run: terminal.observed.run,
        counts: terminal.observed.counts,
        original_ids: terminal.observed.original_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_readback_bootstrap_transfers_original_controller_description_once() {
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let sender = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let receiver = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0u32) };
        assert!(raw >= 0);
        let controller = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        send_bootstrap(sender.as_fd(), controller.as_fd(), [19; 16]).unwrap();
        let (run, received) = receive_bootstrap(receiver.as_fd()).unwrap();
        assert_eq!(run, [19; 16]);
        assert_ne!(received.as_raw_fd(), controller.as_raw_fd());
        assert!(!pidfd_terminal(received.as_fd()).unwrap());
        let mut original: libc::stat = unsafe { std::mem::zeroed() };
        let mut transferred: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::fstat(controller.as_raw_fd(), &mut original) },
            0
        );
        assert_eq!(
            unsafe { libc::fstat(received.as_raw_fd(), &mut transferred) },
            0
        );
        assert_eq!(
            (original.st_dev, original.st_ino),
            (transferred.st_dev, transferred.st_ino)
        );
        send_bootstrap(sender.as_fd(), controller.as_fd(), [0; 16]).unwrap();
        assert!(receive_bootstrap(receiver.as_fd()).is_err());
        assert!(!pidfd_terminal(controller.as_fd()).unwrap());
    }
    #[test]
    fn controller_owned_accepted_readback_does_not_relax_unix_readback_lifetime() {
        let run = "11111111111111111111111111111111";
        let accepted_unit = format!("hermit-accepted-readback-{run}.service");
        let unix_unit = format!("hermit-unix-readback-{run}.service");
        let executable = PathBuf::from("/bin/true");
        let args = ["--accepted-readback-private-stdin-v1".into()];
        let accepted = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::AcceptedReadback,
            unit: &accepted_unit,
            executable: &executable,
            arguments: &args,
            lifetime: CapabilityServiceLifetime::ControllerOwned,
            writable_directories: &[],
        };
        let actual = accepted.arguments().unwrap();
        assert!(
            !actual
                .iter()
                .any(|arg| arg.to_string_lossy().contains("RuntimeMaxSec="))
        );
        let unix = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::UnixReadback,
            unit: &unix_unit,
            ..accepted
        };
        assert!(unix.arguments().is_err());
        assert_eq!(CONTROLLER_CLEANUP_NS, 30_000_000_000);
        assert_eq!(CLOSE_NS, 1_000_000_000);
    }

    #[test]
    fn accepted_retained_service_log_writes_and_reads_same_open_description() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service.log");
        let mut retained = create_retained_service_log(&path).unwrap();
        let mut writer = retained.try_clone().unwrap();
        let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(flags & libc::O_ACCMODE, libc::O_RDWR);
        writer.write_all(b"first service record\n").unwrap();
        writer.flush().unwrap();
        drop(writer);
        assert_eq!(log(&mut retained).unwrap(), "first service record\n");
        assert!(
            create_retained_service_log(&path).is_err(),
            "old receipt was overwritten"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"first service record\n");
    }

    #[test]
    fn accepted_query_and_controller_same_wake_start_deadline_once() {
        let mut deadline = None;
        // Controller is live at the first check; both become ready before
        // poll returns. Use the same transition invoked after actual poll.
        assert!(!observe_readback_wake(false, false, 100, &mut deadline).unwrap());
        assert_eq!(deadline, None);
        assert!(observe_readback_wake(true, true, 110, &mut deadline).unwrap());
        assert_eq!(deadline, Some(110 + CONTROLLER_CLEANUP_NS));
        assert!(!observe_readback_wake(true, false, 120, &mut deadline).unwrap());
        assert_eq!(
            deadline,
            Some(110 + CONTROLLER_CLEANUP_NS),
            "terminal reobservation reset deadline"
        );
        assert!(
            observe_readback_wake(true, true, 110 + CONTROLLER_CLEANUP_NS, &mut deadline).is_err()
        );
        assert!(observe_readback_wake(false, true, 110, &mut None).is_err());
        assert!(observe_readback_wake(true, true, u64::MAX, &mut None).is_err());
        let (query, _) = fixture();
        assert_eq!(query.deadline_ns, query.closed_ns + CLOSE_NS);
    }

    #[test]
    fn accepted_partial_query_custody_refuses_each_incomplete_startup_phase() {
        use std::os::unix::fs::MetadataExt;
        if crate::unix_guard_terminal::isolate_command_parent(
            "accepted_terminal::tests::accepted_partial_query_custody_refuses_each_incomplete_startup_phase",
            true,
        ) {
            return;
        }
        let mut owner = AcceptedParentFinalizer::before_startup(
            tempfile::tempfile().unwrap(),
            tempfile::tempfile().unwrap(),
            PathBuf::from("/bin/true"),
        );
        for phase in ["socketpair", "command-spawn"] {
            assert!(owner.validate_query_custody().is_err(), "{phase}");
            assert!(owner.query.is_none());
        }
        let directory = tempfile::tempdir().unwrap();
        let file = File::open(directory.path()).unwrap();
        let meta = file.metadata().unwrap();
        // The identity below is a state fixture, not native unit evidence.
        let identity = UnitIdentity {
            unit: "fixture.service".into(),
            invocation: "1".repeat(32),
            cgroup: directory.path().to_owned(),
            directory: file,
            device: meta.dev(),
            inode: meta.ino(),
        };
        let mut command = Command::new("/bin/true");
        let mut flight = CommandFlight::start(&mut command).unwrap();
        let child = flight.child.id();
        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        while flight.poll(deadline).unwrap().is_none() {
            pause(deadline).unwrap();
        }
        assert!(flight.status.unwrap().success());
        let endpoint: OwnedFd = tempfile::tempfile().unwrap().into();
        owner.query = Some(("fixture.service".into(), flight, None, endpoint));
        assert!(
            owner.validate_query_custody().is_err(),
            "unit identity capture"
        );
        assert_eq!(owner.query.as_ref().unwrap().1.child.id(), child);
        owner.query.as_mut().unwrap().2 = Some(identity);
        assert!(
            owner.validate_query_custody().is_err(),
            "helper PIDFD capture"
        );
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0u32) };
        assert!(raw >= 0);
        owner.query_task = Some(unsafe { OwnedFd::from_raw_fd(raw as i32) });
        assert!(
            owner.validate_query_custody().is_err(),
            "controller bootstrap send"
        );
        assert!(!pidfd_terminal(owner.query_task.as_ref().unwrap().as_fd()).unwrap());
        owner.query_bootstrapped = true;
        owner.validate_query_custody().unwrap();
        owner.query.as_mut().unwrap().0 = "different.service".into();
        assert!(
            owner.validate_query_custody().is_err(),
            "changed retained unit"
        );
        assert_eq!(owner.query.as_ref().unwrap().1.child.id(), child);
        assert_eq!(
            owner.query.as_ref().unwrap().2.as_ref().unwrap().inode,
            meta.ino()
        );
        assert_eq!(
            unsafe { libc::waitpid(child as i32, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    fn live_bootstrap_helper(case: &str, test: &str) {
        const CHILD: &str = "HERMIT_ACCEPTED_BOOTSTRAP_TEST_CHILD";
        const BEFORE: &str = "HERMIT_ACCEPTED_BOOTSTRAP_TEST_DEADLINE";
        if std::env::var(CHILD).ok().as_deref() == Some(case) {
            let before = std::env::var(BEFORE).unwrap().parse().unwrap();
            let raw = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
            assert!(raw >= 0);
            let input = unsafe { File::from_raw_fd(raw) };
            // This marker precedes the actual production entry; the parent
            // verifies the same owned process remains live before aborting it.
            io::stdout()
                .write_all(b"actual-readback-before-bootstrap\n")
                .unwrap();
            io::stdout().flush().unwrap();
            run_private_readback(Ok(Some(input)), before);
        }
        if crate::unix_guard_terminal::isolate_command_parent(test, true) {
            return;
        }
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let endpoint = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let input = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        let startup = Instant::now() + std::time::Duration::from_secs(1);
        let before_ns = bootstrap_deadline_ns(startup).unwrap();
        // The outer wait is separately bounded; it does not alter the helper's
        // original one-second test startup deadline sent before process spawn.
        let terminal = Instant::now() + std::time::Duration::from_secs(2);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, case)
            .env(BEFORE, before_ns.to_string());
        let mut flight = CommandFlight::start_with_stdin(&mut command, Stdio::from(input)).unwrap();
        let child = flight.child.id();
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child, 0u32) };
        assert!(raw >= 0);
        let pin = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        loop {
            assert!(
                flight.poll(startup).unwrap().is_none(),
                "readback exited before live observation"
            );
            if flight
                .output()
                .unwrap()
                .contains("actual-readback-before-bootstrap\n")
            {
                break;
            }
            pause(startup).unwrap();
        }
        assert!(!pidfd_terminal(pin.as_fd()).unwrap());
        let mut owner = AcceptedParentFinalizer::before_startup(
            tempfile::tempfile().unwrap(),
            tempfile::tempfile().unwrap(),
            std::env::current_exe().unwrap(),
        );
        owner.query = Some(("uncaptured-query.service".into(), flight, None, endpoint));
        owner.query_task = Some(pin);
        if case == "abort" {
            let primary = owner.drain(terminal).unwrap_err();
            assert_eq!(primary.to_string(), "accepted service was never started");
            assert_eq!(
                owner.failure.as_deref(),
                Some("accepted service was never started")
            );
            assert!(owner.query_aborted);
            assert!(
                owner
                    .query_abort_failure
                    .as_ref()
                    .unwrap()
                    .contains("original unit identity")
            );
            assert!(owner.validate_query_custody().is_err());
            assert!(
                owner
                    .drain(terminal)
                    .unwrap_err()
                    .to_string()
                    .contains("cannot restart")
            );
        } else {
            // Deliberately keep the parent endpoint open and send no packet.
            // Only the original absolute bootstrap deadline can end the helper.
            while owner
                .query
                .as_mut()
                .unwrap()
                .1
                .poll(terminal)
                .unwrap()
                .is_none()
            {
                pause(terminal).unwrap();
            }
            assert!(!owner.query_aborted);
            assert!(now_ns().unwrap() >= before_ns);
        }
        assert!(pidfd_terminal(owner.query_task.as_ref().unwrap().as_fd()).unwrap());
        let flight = &mut owner.query.as_mut().unwrap().1;
        assert_eq!(flight.child.id(), child);
        assert_eq!(flight.status.unwrap().code(), Some(125));
        let error = log(&mut flight.stderr).unwrap();
        if case == "abort" {
            assert!(error.contains("bootstrap changed run/controller rights"));
        } else {
            assert!(error.contains("original accepted bootstrap deadline"));
        }
        assert!(
            !flight.output().unwrap().contains("passes_ns"),
            "abort produced an object proof"
        );
        assert!(group_absent(child, terminal).unwrap());
        assert_eq!(
            unsafe { libc::waitpid(child as i32, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn accepted_readback_withheld_bootstrap_expires_at_original_deadline() {
        live_bootstrap_helper(
            "withheld",
            "accepted_terminal::tests::accepted_readback_withheld_bootstrap_expires_at_original_deadline",
        );
    }

    #[test]
    fn accepted_partial_query_abort_drains_original_live_helper_and_preserves_failure() {
        live_bootstrap_helper(
            "abort",
            "accepted_terminal::tests::accepted_partial_query_abort_drains_original_live_helper_and_preserves_failure",
        );
    }

    fn fixture() -> (Query, Vec<Value>) {
        let counts = [22, 45, 50];
        let ids: Vec<_> = counts
            .iter()
            .enumerate()
            .flat_map(|(kind, count)| (1..=*count).map(move |id| (kind as u32, id as u32)))
            .collect();
        let query = Query {
            run: [7; 16],
            ids: ids.clone(),
            counts,
            closed_ns: 100,
            deadline_ns: 100 + CLOSE_NS,
        };
        let inventory = serde_json::json!({"complete":true,"count_invalid":false,
            "status":{"returned":0,"errno":null},
            "ids":ids.iter().map(|(kind,id)| serde_json::json!({"kind":kind,"id":id})).collect::<Vec<_>>()});
        let mut common = serde_json::json!({"schema":"hermit-accepted-provider-terminal-v1",
            "run":query.run,"controller_terminal":true,"requires_external_absence":true});
        common["phase"] = "before_close".into();
        common["failure"] = Value::Null;
        common["inventories"] = serde_json::json!([inventory.clone()]);
        let before = common.clone();
        common.as_object_mut().unwrap().remove("inventories");
        common["phase"] = "after_close".into();
        common["closed_ns"] = query.closed_ns.into();
        common["service_status"] = 0.into();
        common["close_error"] = Value::Null;
        common["socket_release"] = "pending_process_exit".into();
        common["close_receipts"] = serde_json::json!([{"incarnation":u64::from_le_bytes([7;8]),
            "close":{"returned":0,"errno":null},"unexpected_drop":false,
            "requires_external_absence":true,"inventory":inventory}]);
        (query, vec![before, common])
    }
    fn transcript(rows: &[Value]) -> String {
        rows.iter()
            .map(|row| serde_json::to_string(row).unwrap() + "\n")
            .collect()
    }
    #[test]
    fn accepted_terminal_joins_complete_original_population_and_refuses_partial_close() {
        let (query, rows) = fixture();
        query.validate().unwrap();
        assert_eq!(query.ids.len(), 117);
        assert_eq!(
            closed_inventory(&transcript(&rows), query.run, &query.ids).unwrap(),
            query.closed_ns
        );
        for variant in 0..10 {
            let mut bad = rows.clone();
            match variant {
                0 => bad[0]["inventories"] = serde_json::json!([]),
                1 => bad[1]["close_receipts"] = serde_json::json!([]),
                2 => bad[0]["inventories"][0]["ids"]
                    .as_array_mut()
                    .unwrap()
                    .pop()
                    .map(|_| ())
                    .unwrap(),
                3 => bad[1]["close_receipts"][0]["inventory"]["ids"][0]["id"] = 999.into(),
                4 => bad[1]["run"] = serde_json::to_value([8u8; 16]).unwrap(),
                5 => bad[1]["controller_terminal"] = false.into(),
                6 => bad[1]["close_receipts"][0]["unexpected_drop"] = true.into(),
                7 => bad[1]["close_receipts"][0]["close"]["returned"] = (-1).into(),
                8 => bad[1]["closed_ns"] = 0.into(),
                9 => bad.push(bad[1].clone()),
                _ => unreachable!(),
            }
            assert!(
                closed_inventory(&transcript(&bad), query.run, &query.ids).is_err(),
                "{variant}"
            );
        }
        assert!(validate_ids(&[], [0, 0, 0]).is_err());
        assert!(validate_ids(&query.ids, [10, 31, 31]).is_err()); // Unix72 cannot substitute.
    }
    #[test]
    fn accepted_readback_requires_both_full_scans_with_exact_inventory_and_original_deadline() {
        let (query, _) = fixture();
        let good = Readback {
            request: query.clone(),
            passes_ns: [101, 102],
        };
        good.validate(&query).unwrap();
        for variant in 0..6 {
            let mut bad = Readback {
                request: query.clone(),
                passes_ns: good.passes_ns,
            };
            match variant {
                0 => bad.passes_ns[0] = 99,
                1 => bad.passes_ns[1] = 100,
                2 => bad.passes_ns[1] = query.deadline_ns,
                3 => bad.request.ids[0].1 += 500,
                4 => bad.request.deadline_ns += 1,
                5 => bad.request.run[0] += 1,
                _ => unreachable!(),
            }
            assert!(bad.validate(&query).is_err(), "{variant}");
        }
    }
}

fn accepted_directory_names(
    root: &crate::unix_guard_package::RecoveryDeploymentRoot,
) -> io::Result<BTreeSet<String>> {
    // A new descriptor-relative directory stream owns its own seek position.
    // No path or diagnostic descriptor number supplies this custody.
    let fd = unsafe {
        libc::openat(
            root.directory.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        return Err(error);
    }
    struct DirectoryStream(*mut libc::DIR);
    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = DirectoryStream(stream);
    let mut names = BTreeSet::new();
    loop {
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let errno = io::Error::last_os_error();
            if errno.raw_os_error() != Some(0) {
                return Err(errno);
            }
            break;
        }
        let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if bytes.to_bytes() == b"." || bytes.to_bytes() == b".." {
            continue;
        }
        let name = bytes
            .to_str()
            .map_err(|_| io::Error::other("accepted recovery basename is not UTF-8"))?;
        if names.len() >= 384 || !names.insert(name.to_owned()) {
            return Err(io::Error::other(
                "accepted recovery entry population exceeded fixed bound",
            ));
        }
    }
    Ok(names)
}
fn accepted_labels(names: &BTreeSet<String>) -> io::Result<BTreeMap<String, BTreeSet<String>>> {
    let mut labels = BTreeMap::<String, BTreeSet<String>>::new();
    for name in names {
        let suffix = name
            .strip_prefix("accepted-")
            .ok_or_else(|| io::Error::other("unexpected file in accepted recovery root"))?;
        let (label, extension) = suffix
            .split_once('.')
            .ok_or_else(|| io::Error::other("accepted recovery filename lacks role"))?;
        receipt_label(label)?;
        if !matches!(extension, "terminal.jsonl" | "stdout.log" | "stderr.log") {
            return Err(io::Error::other("unexpected accepted recovery file role"));
        }
        if !labels
            .entry(label.to_owned())
            .or_default()
            .insert(extension.to_owned())
        {
            return Err(io::Error::other("accepted recovery repeated file role"));
        }
    }
    Ok(labels)
}
/// Refuse a new load once eight prior launches lack a complete, internally
/// authenticated terminal receipt. This admission check never deletes evidence.
fn admit_accepted_launch(
    root: &crate::unix_guard_package::RecoveryDeploymentRoot,
) -> io::Result<()> {
    let identity = root.identity()?;
    let names = accepted_directory_names(root)?;
    if names.len().checked_add(3).is_none_or(|count| count > 384) {
        return Err(io::Error::other(
            "accepted recovery lacks bounded receipt capacity",
        ));
    }
    let complete_roles = BTreeSet::from([
        "terminal.jsonl".to_owned(),
        "stdout.log".to_owned(),
        "stderr.log".to_owned(),
    ]);
    let mut unresolved = 0usize;
    for (label, roles) in accepted_labels(&names)? {
        let completed = if roles == complete_roles {
            let terminal = receipt_open(root.directory.as_fd(), &receipt_names(&label)[0], false)?;
            let bytes = receipt_read(&terminal)?;
            let first = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|text| text.lines().next())
                .and_then(|row| serde_json::from_str::<BeforeReceipt>(row).ok());
            first.is_some_and(|before| {
                validate_accepted_receipt(root, &label, &before.artifact).is_ok()
            })
        } else {
            false
        };
        if !completed {
            unresolved = unresolved
                .checked_add(1)
                .ok_or_else(|| io::Error::other("accepted unresolved population overflow"))?;
        }
    }
    if unresolved >= MAX_UNRESOLVED_ACCEPTED_LAUNCHES {
        return Err(io::Error::other(
            "accepted unresolved launch admission bound reached",
        ));
    }
    if root.identity()? != identity || accepted_directory_names(root)? != names {
        return Err(io::Error::other(
            "accepted recovery changed during admission census",
        ));
    }
    Ok(())
}
/// Read every entry in an owner's bounded accepted root. Exactly three files
/// per launch and distinct actual run identities are required; guard files or
/// unrelated leftovers are refused, never filtered or exempted.
pub fn validate_accepted_recovery_directory(
    root: &crate::unix_guard_package::RecoveryDeploymentRoot,
    expected: &ProviderArtifact,
) -> io::Result<Vec<AcceptedReceiptReadback>> {
    let identity = root.identity()?;
    let names = accepted_directory_names(root)?;
    let labels = accepted_labels(&names)?
        .into_keys()
        .collect::<BTreeSet<_>>();
    if labels.is_empty() {
        return Err(io::Error::other(
            "accepted recovery census has no completed launch",
        ));
    }
    let declared = labels
        .iter()
        .flat_map(|label| receipt_names(label))
        .collect::<BTreeSet<_>>();
    if names != declared {
        return Err(io::Error::other(
            "accepted recovery three-file population differs",
        ));
    }
    let mut runs = BTreeSet::new();
    let mut receipts = Vec::new();
    for label in labels {
        let receipt = validate_accepted_receipt(root, &label, expected)?;
        if !runs.insert(receipt.run) {
            return Err(io::Error::other(
                "accepted recovery repeated one run under multiple labels",
            ));
        }
        receipts.push(receipt);
    }
    if root.identity()? != identity || accepted_directory_names(root)? != names {
        return Err(io::Error::other(
            "accepted recovery root or population changed during census",
        ));
    }
    Ok(receipts)
}

#[cfg(test)]
mod recovery_receipt_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    fn artifact() -> ProviderArtifact {
        ProviderArtifact {
            topology: detcore::network_runtime::ProviderTopology::GroupedV1 {
                contract_sha256: [9; 32],
            },
            wire_format: detcore::network_runtime::ProviderWireFormat::Abi8Copy5,
            object_sha256: [1; 32],
            library_sha256: [2; 32],
            btf_sha256: [3; 32],
            maps: 23,
            programs: 44,
            links: 44,
        }
    }
    // Metadata grammar fixture only. These files never claim that BPF objects
    // were loaded, that a service ran, or that native cleanup was qualified.
    fn fixture() -> (tempfile::TempDir, AcceptedRecovery, StartupReceipt, Value) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = crate::unix_guard_package::RecoveryDeploymentRoot::open_at(temp.path()).unwrap();
        let mut recovery = AcceptedRecovery::retain(root, "1a".repeat(16));
        recovery.initialize(artifact()).unwrap();
        let run = [7; 16];
        let suffix = "07".repeat(16);
        let actor = |prefix: &str, invocation: &str, inode: u64| {
            let unit = format!("{prefix}{suffix}.service");
            ActorReceipt {
                cgroup: Path::new("/sys/fs/cgroup/system.slice").join(&unit),
                unit,
                invocation: invocation.repeat(16),
                device: 27,
                inode,
            }
        };
        let startup = StartupReceipt {
            schema: "hermit-accepted-parent-startup-v1".into(),
            run,
            artifact: artifact(),
            loader: actor("hermit-accepted-", "08", 31),
            query: actor("hermit-accepted-readback-", "09", 32),
        };
        let counts = [23, 44, 44];
        let ids = counts
            .iter()
            .enumerate()
            .flat_map(|(kind, n)| (1..=*n).map(move |id| (kind as u32, id as u32)))
            .collect::<Vec<_>>();
        let inventory = serde_json::json!({"complete":true,"count_invalid":false,"status":{"returned":0,"errno":null},"ids":ids.iter().map(|(kind,id)|serde_json::json!({"kind":kind,"id":id})).collect::<Vec<_>>()});
        let before = serde_json::json!({"schema":"hermit-accepted-provider-terminal-v1","run":run,"controller_terminal":true,"requires_external_absence":true,"phase":"before_close","failure":null,"inventories":[inventory.clone()]});
        let after = serde_json::json!({"schema":"hermit-accepted-provider-terminal-v1","run":run,"controller_terminal":true,"requires_external_absence":true,"phase":"after_close","closed_ns":100,"service_status":0,"close_error":null,"socket_release":"pending_process_exit","close_receipts":[{"incarnation":u64::from_le_bytes([7;8]),"close":{"returned":0,"errno":null},"unexpected_drop":false,"requires_external_absence":true,"inventory":inventory}]});
        let stdout = format!(
            "{}\n{}\n",
            serde_json::to_string(&before).unwrap(),
            serde_json::to_string(&after).unwrap()
        );
        recovery.files[1]
            .as_mut()
            .unwrap()
            .write_all(stdout.as_bytes())
            .unwrap();
        recovery.files[1].as_ref().unwrap().sync_all().unwrap();
        let terminal = serde_json::json!({"schema":"hermit-accepted-parent-terminal-v1","run":run,"original_ids":ids,"counts":counts,"wrapper_wait":0,"loader":startup.loader,"query_wait":0,"query":startup.query,"readback":{"request":{"run":run,"ids":ids,"counts":counts,"closed_ns":100,"deadline_ns":100+CLOSE_NS},"passes_ns":[101,102]},"terminal_observed_ns":103,"service_stdout":stdout,"service_stderr":""});
        (temp, recovery, startup, terminal)
    }
    #[test]
    fn strict_accepted_receipt_reads_real_files_and_exact_111_id_grammar_fixture() {
        let (_temp, mut r, startup, terminal) = fixture();
        r.started(serde_json::to_value(startup).unwrap()).unwrap();
        r.finish(terminal).unwrap();
        let got = validate_accepted_receipt(&r.root, &r.label, &artifact()).unwrap();
        assert_eq!(got.run, [7; 16]);
        assert_eq!(got.counts, [23, 44, 44]);
        assert_eq!(got.original_ids.len(), 111);
        assert!(r.finish(serde_json::json!({})).is_err());
    }
    #[test]
    fn strict_accepted_receipt_refuses_missing_phase_wrong_actor_log_inventory_and_deadline() {
        for variant in 0..13 {
            let (_temp, mut r, startup, mut terminal) = fixture();
            if variant != 0 {
                r.started(serde_json::to_value(startup).unwrap()).unwrap();
            }
            match variant {
                0 => {}
                1 => {
                    terminal["original_ids"].as_array_mut().unwrap().pop();
                }
                2 => terminal["run"] = serde_json::to_value([8u8; 16]).unwrap(),
                3 => terminal["loader"]["invocation"] = serde_json::json!("0a".repeat(16)),
                4 => terminal["wrapper_wait"] = 1.into(),
                5 => terminal["query_wait"] = 256.into(),
                6 => terminal["readback"]["request"]["deadline_ns"] = (101 + CLOSE_NS).into(),
                7 => terminal["terminal_observed_ns"] = (100 + CLOSE_NS).into(),
                8 => terminal["readback"]["passes_ns"] = serde_json::json!([101]),
                9 => terminal["service_stdout"] = "changed".into(),
                10 => terminal["extra"] = true.into(),
                11 => terminal["readback"]["request"]["ids"][0][1] = 999.into(),
                12 => terminal["counts"] = serde_json::json!([10, 31, 31]),
                _ => unreachable!(),
            }
            assert!(r.finish(terminal).is_err(), "variant {variant}");
            assert!(
                validate_accepted_receipt(&r.root, &r.label, &artifact()).is_err(),
                "variant {variant}"
            );
        }
    }
    #[test]
    fn accepted_receipt_name_replacement_and_symlink_refuse_without_overwrite() {
        let (temp, mut r, startup, terminal) = fixture();
        r.started(serde_json::to_value(startup).unwrap()).unwrap();
        let path = temp.path().join(receipt_names(&r.label)[1].clone());
        let retained = temp.path().join("retained-original");
        std::fs::rename(&path, &retained).unwrap();
        std::os::unix::fs::symlink(&retained, &path).unwrap();
        assert!(r.finish(terminal).is_err());
        assert!(validate_accepted_receipt(&r.root, &r.label, &artifact()).is_err());
        assert_eq!(
            receipt_read(r.files[1].as_ref().unwrap()).unwrap(),
            std::fs::read(retained).unwrap()
        );
        assert!(receipt_open(r.root.directory.as_fd(), &receipt_names(&r.label)[0], true).is_err());
    }
    #[test]
    fn accepted_receipt_rejects_duplicate_failure_extra_rows_and_wrong_package() {
        for variant in 0..5 {
            let (_temp, mut r, startup, terminal) = fixture();
            r.started(serde_json::to_value(startup).unwrap()).unwrap();
            r.finish(terminal).unwrap();
            let file = r.files[0].as_mut().unwrap();
            let original = receipt_read(file).unwrap();
            match variant {
                0 => {
                    file.seek(SeekFrom::End(0)).unwrap();
                    file.write_all(b"{\"schema\":1,\"stage\":\"accepted_failed\"}\n")
                        .unwrap();
                }
                1 => {
                    let text = String::from_utf8(original).unwrap();
                    let replaced = text.replacen("\"schema\":1", "\"schema\":1,\"schema\":1", 1);
                    file.set_len(0).unwrap();
                    file.seek(SeekFrom::Start(0)).unwrap();
                    file.write_all(replaced.as_bytes()).unwrap();
                }
                2 => {
                    let text = String::from_utf8(original).unwrap();
                    let row = text.lines().last().unwrap().to_owned();
                    file.seek(SeekFrom::End(0)).unwrap();
                    writeln!(file, "{row}").unwrap();
                }
                3 => {
                    r.files[1].as_mut().unwrap().write_all(b"extra").unwrap();
                }
                4 => {}
                _ => unreachable!(),
            }
            let mut expected = artifact();
            if variant == 4 {
                expected.object_sha256 = [8; 32];
            }
            assert!(
                validate_accepted_receipt(&r.root, &r.label, &expected).is_err(),
                "variant {variant}"
            );
        }
    }
    #[test]
    fn accepted_census_refuses_extra_file_and_duplicate_run_without_guard_exemptions() {
        for duplicate in [false, true] {
            let (temp, mut r, startup, terminal) = fixture();
            r.started(serde_json::to_value(startup.clone()).unwrap())
                .unwrap();
            r.finish(terminal.clone()).unwrap();
            assert_eq!(
                validate_accepted_recovery_directory(&r.root, &artifact())
                    .unwrap()
                    .len(),
                1
            );
            if duplicate {
                let root = crate::unix_guard_package::RecoveryDeploymentRoot::open_at(temp.path())
                    .unwrap();
                let mut second = AcceptedRecovery::retain(root, "1b".repeat(16));
                second.initialize(artifact()).unwrap();
                second.files[1]
                    .as_mut()
                    .unwrap()
                    .write_all(terminal["service_stdout"].as_str().unwrap().as_bytes())
                    .unwrap();
                second
                    .started(serde_json::to_value(startup).unwrap())
                    .unwrap();
                second.finish(terminal).unwrap();
                let error = validate_accepted_recovery_directory(&r.root, &artifact()).unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "accepted recovery repeated one run under multiple labels"
                );
            } else {
                std::fs::write(temp.path().join("foreign.guard"), b"not accepted").unwrap();
                assert_eq!(
                    validate_accepted_recovery_directory(&r.root, &artifact())
                        .unwrap_err()
                        .to_string(),
                    "unexpected file in accepted recovery root"
                );
            }
        }
    }

    #[test]
    fn accepted_recovery_refuses_uninitialized_operations_without_panicking() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = crate::unix_guard_package::RecoveryDeploymentRoot::open_at(temp.path()).unwrap();
        let mut r = AcceptedRecovery::retain(root, "1a".repeat(16));
        assert!(r.service_logs().is_err());
        assert!(r.started(serde_json::json!({})).is_err());
        assert!(r.finish(serde_json::json!({})).is_err());
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }
    #[test]
    fn accepted_partial_creation_retains_each_opened_file_and_latches_failure() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = crate::unix_guard_package::RecoveryDeploymentRoot::open_at(temp.path()).unwrap();
        let mut r = AcceptedRecovery::retain(root, "1a".repeat(16));
        let stdout = temp.path().join(&receipt_names(&r.label)[1]);
        std::fs::write(&stdout, b"existing evidence").unwrap();
        let error = r.initialize(artifact()).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EEXIST));
        assert!(r.files[0].is_some());
        assert!(r.files[1].is_none() && r.files[2].is_none());
        assert_eq!(std::fs::read(&stdout).unwrap(), b"existing evidence");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 2);
        assert!(
            r.initialize(artifact())
                .unwrap_err()
                .to_string()
                .contains("cannot restart")
        );
        assert!(r.service_logs().is_err());
        assert!(r.started(serde_json::json!({})).is_err());
        r.failure("preserved partial initialization").unwrap();
        assert!(
            String::from_utf8(receipt_read(r.files[0].as_ref().unwrap()).unwrap())
                .unwrap()
                .contains("accepted_failed")
        );
        assert!(validate_accepted_recovery_directory(&r.root, &artifact()).is_err());
    }
    #[test]
    fn accepted_admission_bounds_unresolved_launches_without_deleting_evidence() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        for ordinal in 1..=MAX_UNRESOLVED_ACCEPTED_LAUNCHES {
            let label = format!("{ordinal:032x}");
            std::fs::write(
                temp.path().join(receipt_names(&label)[0].clone()),
                b"retained failure\n",
            )
            .unwrap();
        }
        let before = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<BTreeSet<_>>();
        let root = crate::unix_guard_package::RecoveryDeploymentRoot::open_at(temp.path()).unwrap();
        let mut refused = AcceptedRecovery::retain(root, "f1".repeat(16));
        assert_eq!(
            refused.initialize(artifact()).unwrap_err().to_string(),
            "accepted unresolved launch admission bound reached"
        );
        assert_eq!(
            std::fs::read_dir(temp.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<BTreeSet<_>>(),
            before
        );
        std::fs::remove_file(
            temp.path().join(
                receipt_names(&format!("{:032x}", MAX_UNRESOLVED_ACCEPTED_LAUNCHES))[0].clone(),
            ),
        )
        .unwrap();
        let root = crate::unix_guard_package::RecoveryDeploymentRoot::open_at(temp.path()).unwrap();
        let mut admitted = AcceptedRecovery::retain(root, "f2".repeat(16));
        admitted.initialize(artifact()).unwrap();
        assert_eq!(
            std::fs::read_dir(temp.path()).unwrap().count(),
            MAX_UNRESOLVED_ACCEPTED_LAUNCHES - 1 + 3
        );
    }
    #[test]
    fn accepted_file_readback_refuses_fifo_mode_hardlink_missing_and_replaced_inode() {
        for variant in 0..5 {
            let (temp, mut r, startup, terminal) = fixture();
            r.started(serde_json::to_value(startup).unwrap()).unwrap();
            r.finish(terminal).unwrap();
            let path = temp.path().join(&receipt_names(&r.label)[2]);
            match variant {
                0 => {
                    std::fs::remove_file(&path).unwrap();
                    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
                1 => {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap()
                }
                2 => std::fs::hard_link(&path, temp.path().join("second-link")).unwrap(),
                3 => std::fs::remove_file(&path).unwrap(),
                4 => {
                    std::fs::rename(&path, temp.path().join("old-inode")).unwrap();
                    File::options()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&path)
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let error = validate_accepted_receipt(&r.root, &r.label, &artifact()).unwrap_err();
            match variant {
                0 | 1 | 2 => assert_eq!(
                    error.to_string(),
                    "accepted receipt file shape/owner/extent differs"
                ),
                3 => assert_eq!(error.raw_os_error(), Some(libc::ENOENT)),
                4 => assert_eq!(
                    error.to_string(),
                    "accepted file differs from original launch identity"
                ),
                _ => unreachable!(),
            }
        }
    }
    fn replace_fixture_stdout(r: &mut AcceptedRecovery, terminal: &mut Value, text: &str) {
        let file = r.files[1].as_mut().unwrap();
        file.set_len(0).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(text.as_bytes()).unwrap();
        file.sync_all().unwrap();
        terminal["service_stdout"] = text.into();
    }
    #[test]
    fn accepted_service_transcript_refuses_missing_null_duplicate_keys_and_partial_line() {
        for variant in 0..8 {
            let (_temp, mut r, startup, mut terminal) = fixture();
            let mut rows = terminal["service_stdout"]
                .as_str()
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            match variant {
                0 => {
                    rows[0].as_object_mut().unwrap().remove("failure");
                }
                1 => {
                    rows[1].as_object_mut().unwrap().remove("close_error");
                }
                2 => {
                    rows[0]["inventories"][0]["status"]
                        .as_object_mut()
                        .unwrap()
                        .remove("errno");
                }
                3 => {
                    rows[1]["close_receipts"][0]["close"]
                        .as_object_mut()
                        .unwrap()
                        .remove("errno");
                }
                4 => {
                    rows[1]["close_receipts"][0]["inventory"]["status"]
                        .as_object_mut()
                        .unwrap()
                        .remove("errno");
                }
                _ => {}
            }
            let mut text = rows
                .iter()
                .map(|row| format!("{}\n", serde_json::to_string(row).unwrap()))
                .collect::<String>();
            if variant == 5 {
                text = text.replacen("\"failure\":null", "\"failure\":null,\"failure\":null", 1);
            }
            if variant == 6 {
                text = text.replacen("\"kind\":0", "\"kind\":0,\"kind\":0", 1);
            }
            if variant == 7 {
                text.pop();
            }
            replace_fixture_stdout(&mut r, &mut terminal, &text);
            r.started(serde_json::to_value(startup).unwrap()).unwrap();
            let error = r.finish(terminal).unwrap_err().to_string();
            let expected = match variant {
                0 | 1 => "lacks explicit clean error fields",
                2 => "inventory lacks explicit null errno",
                3 | 4 => "close lacks explicit null errno",
                5 | 6 => "duplicate accepted receipt key",
                7 => "transcript is partial",
                _ => unreachable!(),
            };
            assert!(error.contains(expected), "variant {variant}: {error}");
            assert!(validate_accepted_receipt(&r.root, &r.label, &artifact()).is_err());
        }
    }
    #[test]
    fn accepted_terminal_retains_exact_nonzero_unique_ids_and_two_ordered_full_passes() {
        for variant in 0..8 {
            let (_temp, mut r, startup, mut terminal) = fixture();
            r.started(serde_json::to_value(startup).unwrap()).unwrap();
            match variant {
                0 => terminal["original_ids"][0][1] = 0.into(),
                1 => terminal["original_ids"][0][0] = 3.into(),
                2 => terminal["original_ids"][1] = terminal["original_ids"][0].clone(),
                3 => terminal["readback"]["passes_ns"] = serde_json::json!([99, 102]),
                4 => terminal["readback"]["passes_ns"] = serde_json::json!([102, 101]),
                5 => terminal["readback"]["passes_ns"] = serde_json::json!([101, 100 + CLOSE_NS]),
                6 => terminal["terminal_observed_ns"] = 101.into(),
                7 => terminal["readback"]["request"]["closed_ns"] = 0.into(),
                _ => unreachable!(),
            }
            let error = r.finish(terminal).unwrap_err().to_string();
            let expected = match variant {
                0..=2 => "partial, duplicate or invalid accepted original ID",
                3..=5 | 7 => "accepted readback changed original inventory or deadline",
                6 => "accepted terminal receipt exceeds original deadline",
                _ => unreachable!(),
            };
            assert!(error.contains(expected), "variant {variant}: {error}");
        }
    }
    #[test]
    fn accepted_complete_census_accepts_two_distinct_bound_runs() {
        let (temp, mut first, startup, terminal) = fixture();
        first
            .started(serde_json::to_value(startup.clone()).unwrap())
            .unwrap();
        first.finish(terminal.clone()).unwrap();
        let root = crate::unix_guard_package::RecoveryDeploymentRoot::open_at(temp.path()).unwrap();
        let mut second = AcceptedRecovery::retain(root, "1b".repeat(16));
        second.initialize(artifact()).unwrap();
        let mut next = startup.clone();
        next.run = [8; 16];
        next.loader.unit = format!("hermit-accepted-{}.service", "08".repeat(16));
        next.query.unit = format!("hermit-accepted-readback-{}.service", "08".repeat(16));
        next.loader.cgroup = Path::new("/sys/fs/cgroup/system.slice").join(&next.loader.unit);
        next.query.cgroup = Path::new("/sys/fs/cgroup/system.slice").join(&next.query.unit);
        next.loader.invocation = "0a".repeat(16);
        next.query.invocation = "0b".repeat(16);
        next.loader.inode = 41;
        next.query.inode = 42;
        let mut proof = terminal.clone();
        proof["run"] = serde_json::json!(next.run);
        proof["loader"] = serde_json::to_value(&next.loader).unwrap();
        proof["query"] = serde_json::to_value(&next.query).unwrap();
        proof["readback"]["request"]["run"] = serde_json::json!(next.run);
        let mut rows = proof["service_stdout"]
            .as_str()
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        for row in &mut rows {
            row["run"] = serde_json::json!(next.run);
        }
        rows[1]["close_receipts"][0]["incarnation"] = u64::from_le_bytes([8; 8]).into();
        let text = rows
            .iter()
            .map(|row| format!("{}\n", serde_json::to_string(row).unwrap()))
            .collect::<String>();
        replace_fixture_stdout(&mut second, &mut proof, &text);
        second.started(serde_json::to_value(next).unwrap()).unwrap();
        second.finish(proof).unwrap();
        let census = validate_accepted_recovery_directory(&first.root, &artifact()).unwrap();
        assert_eq!(census.len(), 2);
        assert_eq!(
            census.iter().map(|r| r.run).collect::<BTreeSet<_>>(),
            BTreeSet::from([[7; 16], [8; 16]])
        );
        assert!(
            census
                .iter()
                .all(|r| r.original_ids.len() == 111 && r.counts == [23, 44, 44])
        );
    }
    #[test]
    fn accepted_census_refuses_missing_roles_and_partial_receipt_without_relabelling() {
        for variant in 0..3 {
            let (temp, mut r, startup, terminal) = fixture();
            r.started(serde_json::to_value(startup).unwrap()).unwrap();
            r.finish(terminal).unwrap();
            match variant {
                0 => std::fs::remove_file(temp.path().join(&receipt_names(&r.label)[2])).unwrap(),
                1 => {
                    let file = r.files[0].as_ref().unwrap();
                    file.set_len(file.metadata().unwrap().len() - 1).unwrap();
                }
                2 => {
                    let file = r.files[0].as_mut().unwrap();
                    file.set_len(0).unwrap();
                }
                _ => unreachable!(),
            }
            let error = validate_accepted_recovery_directory(&r.root, &artifact())
                .unwrap_err()
                .to_string();
            assert_eq!(
                error,
                if variant == 0 {
                    "accepted recovery three-file population differs"
                } else {
                    "accepted receipt incomplete/failed/extra row"
                }
            );
        }
    }

    #[test]
    fn accepted_terminal_refuses_each_original_loader_and_query_identity_change() {
        for actor in ["loader", "query"] {
            for field in ["unit", "invocation", "cgroup", "device", "inode"] {
                let (_temp, mut r, startup, mut terminal) = fixture();
                r.started(serde_json::to_value(startup).unwrap()).unwrap();
                terminal[actor][field] = match field {
                    "unit" => "other.service".into(),
                    "invocation" => "0e".repeat(16).into(),
                    "cgroup" => "/sys/fs/cgroup/other.service".into(),
                    "device" | "inode" => 999.into(),
                    _ => unreachable!(),
                };
                assert_eq!(
                    r.finish(terminal).unwrap_err().to_string(),
                    "accepted terminal changed original startup/log/actor identity",
                    "{actor}.{field}"
                );
            }
        }
    }
    #[test]
    fn accepted_startup_refuses_aliases_wrong_run_units_and_partial_grouped_inventory() {
        for variant in 0..5 {
            let (_temp, mut r, mut startup, _) = fixture();
            match variant {
                0 => startup.query.invocation = startup.loader.invocation.clone(),
                1 => {
                    startup.query.device = startup.loader.device;
                    startup.query.inode = startup.loader.inode;
                }
                2 => startup.run = [8; 16],
                3 => {
                    startup.artifact.links = 43;
                    r.artifact.as_mut().unwrap().links = 43;
                }
                4 => {
                    startup.loader.invocation = "00".repeat(16);
                }
                _ => unreachable!(),
            }
            let error = r
                .started(serde_json::to_value(startup).unwrap())
                .unwrap_err()
                .to_string();
            let expected = match variant {
                0 | 1 => "accepted actor identities alias",
                2 => "accepted actor units differ from actual startup run",
                3 => "accepted receipt artifact population or identity differs",
                4 => "accepted receipt label differs",
                _ => unreachable!(),
            };
            assert_eq!(error, expected, "variant {variant}");
            assert!(r.service_logs().is_err());
            assert!(r.finish(serde_json::json!({})).is_err());
        }
    }
}
