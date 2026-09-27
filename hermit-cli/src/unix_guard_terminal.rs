//! Parent-owned Unix policy drain. A provisional policy snapshot, object-ID
//! absence and process/unit termination are independent receipts. This owner
//! retains all of them, and every incomplete command, before returning evidence.

#[path = "unix_guard_process.rs"]
pub(crate) mod process;
use std::io::{self};
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::time::Duration;
use std::time::Instant;

use detcore::network_runtime::capability_unit::CAPABILITY_ENVIRONMENT;
use detcore::network_runtime::capability_unit::CAPABILITY_SUDO;
use detcore::network_runtime::capability_unit::CapabilityServiceKind;
use detcore::network_runtime::capability_unit::CapabilityServiceLifetime;
use detcore::network_runtime::capability_unit::CapabilityUnitLaunch;
use process::CommandFlight;
pub use process::UnitDrainEvidence;
use process::UnitIdentity;
use process::group_absent;
use process::pause;
use process::within;

use crate::unix_guard::GuardBirth;
use crate::unix_guard::GuardProvisionalReceipt;
use crate::unix_guard::GuardReadbackCertificate;
use crate::unix_guard::GuardReadbackClient;
use crate::unix_guard::ParentGuard;
use crate::unix_guard_package::PackagedUnixGuard;

/// Establish descendant wait ownership before launching network helper commands.
///
/// This is a process-lifetime responsibility: the caller must not clear the
/// subreaper flag or install a competing waiter for these owned process groups.
/// Repeated calls are idempotent. Other child owners retain their exact waits;
/// command cleanup never uses an unrestricted wait for any child.
pub fn prepare_command_parent() -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    process::require_command_parent()
}

fn pidfd_terminal(fd: BorrowedFd<'_>) -> io::Result<bool> {
    let mut p = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut p, 1, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if p.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
        return Err(io::Error::other("invalid retained helper pidfd"));
    }
    Ok(p.revents & libc::POLLIN != 0)
}

/// Completed guard drain, not a successful guest result. The aggregate caller
/// still retains its real child result and checks full initial-task enrollment.
#[derive(Debug)]
pub struct GuardDrainEvidence {
    pub birth: Option<GuardBirth>,
    pub provisional: GuardProvisionalReceipt,
    pub original_ids: Vec<(u32, u32)>,
    pub counts: [u32; 3],
    pub readback: GuardReadbackCertificate,
    pub loader: UnitDrainEvidence,
    pub query: UnitDrainEvidence,
    pub loader_wait: i32,
    pub query_wait: i32,
    /// CLOCK_MONOTONIC observation after every actor/unit/group has drained.
    pub terminal_observed_ns: u64,
}

struct QueryOwner {
    unit: String,
    child: Option<Child>,
    status: Option<ExitStatus>,
    client: Option<GuardReadbackClient>,
    endpoint: Option<OwnedFd>,
    identity: Option<UnitIdentity>,
}

/// Owns the actual loader and readback actors, original IDs and every command
/// on errors. A failed drain cannot be retried into a fresh successful window.
#[must_use = "retain incomplete guard actors and recovery evidence"]
pub struct GuardParentFinalizer {
    guard: ParentGuard,
    readback_executable: PathBuf,
    stdout: OwnedFd,
    stderr: OwnedFd,
    deadline: Option<Instant>,
    loader: Option<UnitIdentity>,
    query: Option<QueryOwner>,
    commands: Vec<CommandFlight>,
    failure: Option<String>,
    evidence: Option<GuardDrainEvidence>,
}
impl GuardParentFinalizer {
    pub fn new(
        guard: ParentGuard,
        package: &PackagedUnixGuard,
        stdout: OwnedFd,
        stderr: OwnedFd,
    ) -> Self {
        Self {
            guard,
            readback_executable: package.readback.clone(),
            stdout,
            stderr,
            deadline: None,
            loader: None,
            query: None,
            commands: Vec::new(),
            failure: None,
            evidence: None,
        }
    }
    pub fn guard(&self) -> &ParentGuard {
        &self.guard
    }
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }
    /// Exact owned unit names for durable recovery even after failed launch or
    /// an unknown terminal response. These names alone are not drain evidence.
    pub fn recovery_units(&self) -> (&str, Option<&str>) {
        (
            self.guard.unit(),
            self.query.as_ref().map(|query| query.unit.as_str()),
        )
    }

    fn command(
        &mut self,
        privileged: bool,
        args: &[&str],
        deadline: Instant,
    ) -> io::Result<String> {
        within(deadline)?;
        if self.commands.len() >= 64 {
            return Err(io::Error::other("terminal command census exhausted"));
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
        let flight = self.commands.last_mut().expect("stored command");
        loop {
            if let Some(status) = flight.poll(deadline)? {
                within(deadline)?;
                let out = flight.output()?;
                within(deadline)?;
                // `show` returns 1 for a missing unit; its exact structured
                // LoadState is inspected by the caller, never inferred from rc.
                if !status.success() && !(args.first() == Some(&"show") && status.code() == Some(1))
                {
                    return Err(io::Error::other(format!("unit command failed: {status}")));
                }
                return Ok(out);
            }
            if let Err(error) = within(deadline) {
                let _ = flight.kill_group();
                return Err(error);
            }
            pause(deadline)?;
        }
    }
    fn show(&mut self, unit: &str, deadline: Instant) -> io::Result<String> {
        self.command(
            false,
            &[
                "show",
                "--no-pager",
                "--property=Id,LoadState,ActiveState,SubState,ControlGroup,InvocationID,MainPID",
                unit,
            ],
            deadline,
        )
    }
    fn drain_unit(
        &mut self,
        identity: &UnitIdentity,
        deadline: Instant,
    ) -> io::Result<UnitDrainEvidence> {
        self.command(
            true,
            &["--no-ask-password", "stop", &identity.unit],
            deadline,
        )?;
        loop {
            let text = self.show(&identity.unit, deadline)?;
            if identity.drained(&text)? {
                within(deadline)?;
                return Ok(identity.receipt());
            }
            pause(deadline)?;
        }
    }
    fn start_query(&mut self, deadline: Instant) -> io::Result<()> {
        let (_, absolute_ns) = self.guard.terminal_deadline()?;
        let suffix = self
            .guard
            .unit()
            .strip_prefix("hermit-unix-")
            .ok_or_else(|| io::Error::other("loader unit identity"))?;
        let unit = format!("hermit-unix-readback-{suffix}");
        let mut pair = [-1; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let endpoint = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let helper = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        self.query = Some(QueryOwner {
            unit: unit.clone(),
            child: None,
            status: None,
            client: None,
            endpoint: Some(endpoint),
            identity: None,
        });
        let arguments = [
            "--readback-before-ns".into(),
            absolute_ns.to_string().into(),
        ];
        let launch = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::UnixReadback,
            unit: &unit,
            executable: &self.readback_executable,
            arguments: &arguments,
            lifetime: CapabilityServiceLifetime::Bounded(30),
            writable_directories: &[],
        };
        let mut command = launch.command(&helper, &self.stdout, &self.stderr)?;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        self.query.as_mut().expect("query owner").child = Some(child);
        drop(helper);
        let channel = self
            .query
            .as_mut()
            .expect("query owner")
            .endpoint
            .take()
            .expect("private endpoint");
        let prepared = unsafe { GuardReadbackClient::prepare(channel, &self.guard) };
        match prepared {
            Ok(client) => self.query.as_mut().expect("query owner").client = Some(client),
            Err(failure) => {
                self.query.as_mut().expect("query owner").client = Some(failure.owner);
                return Err(failure.error);
            }
        }
        let identity = UnitIdentity::capture(&unit, &self.show(&unit, deadline)?)?;
        self.query.as_mut().expect("query owner").identity = Some(identity);
        Ok(())
    }
    /// Drain after settling the actual Container child. `close_terminal` also
    /// verifies its held controller pidfd; a running controller cannot pass.
    /// An unarmed loader can use this for cleanup, but its zero-task receipt
    /// cannot satisfy the aggregate guest-success population check.
    pub fn drain(&mut self, deadline: Instant) -> io::Result<&GuardDrainEvidence> {
        if let Some(error) = &self.failure {
            return Err(io::Error::other(error.clone()));
        }
        if self.evidence.is_none() {
            let deadline = *self.deadline.get_or_insert(deadline);
            match self.drain_once(deadline) {
                Ok(evidence) => self.evidence = Some(evidence),
                Err(error) => {
                    self.failure = Some(error.to_string());
                    return Err(error);
                }
            }
        }
        Ok(self.evidence.as_ref().expect("complete evidence"))
    }
    fn drain_once(&mut self, deadline: Instant) -> io::Result<GuardDrainEvidence> {
        within(deadline)?;
        let unit = self.guard.unit().to_owned();
        self.loader = Some(UnitIdentity::capture(&unit, &self.show(&unit, deadline)?)?);
        let provisional = loop {
            match self.guard.prepare_terminal(deadline) {
                Ok(receipt) => break receipt,
                Err(error) if matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EBUSY)) => {
                    pause(deadline)?
                }
                Err(error) => return Err(error),
            }
        };
        let original_ids = self
            .guard
            .original_ids()
            .ok_or_else(|| io::Error::other("missing original ID inventory"))?;
        let counts = self
            .guard
            .original_id_counts()
            .ok_or_else(|| io::Error::other("missing original ID counts"))?;
        self.start_query(deadline)?;
        let closed = self.guard.close_terminal()?;
        let sampled = Instant::now();
        let mut now: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let now_ns = (now.tv_sec as u64)
            .checked_mul(1_000_000_000)
            .and_then(|v| v.checked_add(now.tv_nsec as u64))
            .ok_or_else(|| io::Error::other("terminal clock overflow"))?;
        let remaining = closed.deadline_ns().checked_sub(now_ns).ok_or_else(|| {
            io::Error::new(io::ErrorKind::TimedOut, "original object-close deadline")
        })?;
        let close_deadline = sampled
            .checked_add(Duration::from_nanos(remaining))
            .ok_or_else(|| io::Error::other("terminal deadline overflow"))?
            .min(deadline);
        // The readback client retains the stricter close+1s deadline. Every
        // process/unit observation below consumes that same time, never renews it.
        while !self.guard.keeper_is_terminal()? {
            pause(close_deadline)?;
        }
        let loader_identity = self.loader.take().expect("loader identity");
        let loader_result = self.drain_unit(&loader_identity, close_deadline);
        self.loader = Some(loader_identity);
        let loader = loader_result?;
        let loader_wait = loop {
            if let Some(status) = self.guard.poll_launcher_terminal()? {
                break status;
            }
            pause(close_deadline)?;
        };
        within(close_deadline)?;
        if loader_wait != 0 {
            return Err(io::Error::other(format!(
                "loader launcher failed: {loader_wait}"
            )));
        }
        while !group_absent(self.guard.launcher_group_id() as u32, close_deadline)? {
            pause(close_deadline)?;
        }
        let readback = self
            .query
            .as_mut()
            .expect("query owner")
            .client
            .as_mut()
            .expect("query client")
            .check(closed)?;
        loop {
            let client = self
                .query
                .as_ref()
                .expect("query owner")
                .client
                .as_ref()
                .expect("query client");
            if pidfd_terminal(
                client
                    .helper_pidfd()
                    .ok_or_else(|| io::Error::other("query helper pidfd absent"))?,
            )? {
                break;
            }
            pause(close_deadline)?;
        }
        let query_identity = self
            .query
            .as_mut()
            .expect("query owner")
            .identity
            .take()
            .expect("query identity");
        let query_result = self.drain_unit(&query_identity, close_deadline);
        self.query.as_mut().expect("query owner").identity = Some(query_identity);
        let query = query_result?;
        let query_wait = loop {
            let owner = self.query.as_mut().expect("query owner");
            if let Some(status) = owner.child.as_mut().expect("query wrapper").try_wait()? {
                owner.status = Some(status);
                break status;
            }
            pause(close_deadline)?;
        };
        if !query_wait.success() {
            return Err(io::Error::other(format!(
                "readback launcher failed: {query_wait}"
            )));
        }
        let group = self
            .query
            .as_ref()
            .expect("query owner")
            .child
            .as_ref()
            .expect("query wrapper")
            .id();
        while !group_absent(group, close_deadline)? {
            pause(close_deadline)?;
        }
        within(close_deadline)?;
        let mut terminal_time: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut terminal_time) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let terminal_observed_ns = (terminal_time.tv_sec as u64)
            .checked_mul(1_000_000_000)
            .and_then(|v| v.checked_add(terminal_time.tv_nsec as u64))
            .ok_or_else(|| io::Error::other("terminal observation clock overflow"))?;
        if terminal_observed_ns >= readback.deadline_ns() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "actor drain observed after original object-close deadline",
            ));
        }
        within(close_deadline)?;
        Ok(GuardDrainEvidence {
            birth: self.guard.birth(),
            provisional,
            original_ids,
            counts,
            readback,
            loader,
            query,
            loader_wait,
            query_wait: query_wait.into_raw(),
            terminal_observed_ns,
        })
    }
}

// Isolate the process-wide subreaper flag from concurrently running libtests.
// The successful inner body must drain its own CommandFlights; this wrapper
// cannot adopt grandchildren on its behalf while that body remains alive.
#[cfg(test)]
pub(crate) fn isolate_command_parent(exact: &str, prepare: bool) -> bool {
    const CHILD: &str = "HERMIT_COMMAND_PARENT_TEST_CHILD";
    if std::env::var(CHILD).ok().as_deref() == Some(exact) {
        let mut enabled: libc::c_int = -1;
        assert_eq!(unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut enabled, 0, 0, 0) }, 0);
        assert_eq!(enabled, 0, "subreaper ownership is not inherited across fork");
        if prepare {
            prepare_command_parent().unwrap();
            prepare_command_parent().unwrap();
            process::require_command_parent().unwrap();
        }
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", exact, "--test-threads=1", "--nocapture"])
        .env(CHILD, exact)
        .process_group(0)
        .spawn()
        .unwrap();
    let mut terminal = false;
    while Instant::now() < deadline {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::waitid(libc::P_PID, child.id(), &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) }, 0);
        let pid = unsafe { info.si_pid() };
        if pid != 0 {
            assert_eq!(pid, child.id() as i32);
            terminal = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // Keep the original child unreaped through the only delivered group signal.
    // A timeout remains a failure even if the following owned cleanup succeeds.
    let killed = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    if killed < 0 {
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
    let actual = child.wait().unwrap();
    assert!(terminal && actual.success(), "isolated command parent: {actual}");
    assert_eq!(unsafe { libc::kill(-(child.id() as i32), 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    assert_eq!(unsafe { libc::waitpid(child.id() as i32, std::ptr::null_mut(), libc::WNOHANG) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_without_subreaper_refuses_before_spawning() {
        if isolate_command_parent("unix_guard_terminal::tests::command_without_subreaper_refuses_before_spawning", false) {
            return;
        }
        let no_children = || {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::waitid(libc::P_ALL, 0, &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
        };
        no_children();
        let error = CommandFlight::start(&mut Command::new("/bin/true"))
            .err().expect("unprepared command parent spawned a child");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "command parent must retain child-subreaper ownership");
        no_children();
    }

    #[test]
    fn command_waits_for_descendant_absence_after_leader_reap() {
        if isolate_command_parent("unix_guard_terminal::tests::command_waits_for_descendant_absence_after_leader_reap", true) {
            return;
        }
        let mut flight = CommandFlight::start(
            Command::new("/bin/sh").args(["-c", "sleep 30 & printf '%s\\n' $!; sleep 0.1; exit 0"]),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while flight.stdout.metadata().unwrap().len() == 0 {
            pause(deadline).unwrap();
        }
        let descendant: u32 = flight.output().unwrap().trim().parse().unwrap();
        assert!(Path::new(&format!("/proc/{descendant}")).exists());
        let mut observed = false;
        loop {
            let result = flight
                .poll_with_group_observation(deadline, |_, _| {
                    observed = true;
                    Ok(false)
                })
                .unwrap();
            assert!(
                result.is_none(),
                "leader wait alone cannot certify the group"
            );
            if observed {
                break;
            }
            pause(deadline).unwrap();
        }
        assert!(flight.status.unwrap().success());
        loop {
            if let Some(status) = flight.poll(deadline).unwrap() {
                assert!(status.success());
                break;
            }
            pause(deadline).unwrap();
        }
        assert!(group_absent(flight.child.id(), deadline).unwrap());
        assert!(!Path::new(&format!("/proc/{descendant}")).exists());
        assert_eq!(flight.adopted, [(descendant as i32, libc::SIGKILL)]);
        process::require_command_parent().unwrap();
    }

    #[test]
    fn command_observed_after_original_deadline_stays_failed() {
        if isolate_command_parent("unix_guard_terminal::tests::command_observed_after_original_deadline_stays_failed", true) {
            return;
        }
        let mut flight = CommandFlight::start(&mut Command::new("/bin/true")).unwrap();
        let deadline = Instant::now();
        let recovery = Instant::now() + Duration::from_secs(2);
        loop {
            match flight.poll_with_group_observation(deadline, |_, _| Ok(true)) {
                Ok(None) => pause(recovery).unwrap(),
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                    break;
                }
                Ok(Some(_)) => panic!("late successful wait cannot meet the original deadline"),
            }
        }
        // The primary failure above is retained by GuardParentFinalizer. This
        // separate check observes only safe cleanup of this test's real child.
        while flight.poll(recovery).unwrap().is_none() {
            pause(recovery).unwrap();
        }
        assert!(flight.status.unwrap().success());
        assert!(group_absent(flight.child.id(), recovery).unwrap());
    }
}
