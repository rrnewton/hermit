//! Parent-side custody captured from the original trusted launch, before any
//! source admission. This is neither an SCM Creator nor provider authority.
//! The caller retains the actual Source Launcher and never recreates its fresh
//! unit nonce while this lease exists. Every native acquisition stays here on
//! failure; query/creator/cgroup evidence cannot be reconstructed from JSON.
use std::ffi::CString;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::time::Instant;

use serde_json::json;

use super::Failure;
use super::owner;
use super::require;
use super::wire;

#[derive(Clone, Debug, Eq, PartialEq)]
struct NativeIdentity {
    pid: libc::pid_t,
    invocation: String,
    cgroup: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Retained,
    InitialQuery,
    RecheckQuery,
    Captured,
}
/// Read-only borrows of an actual completed launch capture. This observation
/// grants no Creator, stop, wait, cgroup-unlink or SourceTerminal authority.
pub(super) struct CapturedNativeInputs<'a> {
    pub unit: &'a str,
    pub nonce: &'a str,
    pub pid: libc::pid_t,
    pub invocation: &'a str,
    pub cgroup: &'a str,
    pub pidfd: BorrowedFd<'a>,
    pub directory: BorrowedFd<'a>,
    pub directory_identity: &'a owner::FileIdentity,
    pub snapshots: [&'a owner::ManagerSnapshot; 2],
    pub launcher: &'a owner::LauncherLease,
    pub original_deadline: Instant,
    pub first_failure: Option<&'a Failure>,
    pub failure_origin: Option<u64>,
}
#[derive(Debug)]
#[must_use = "parent launch custody survives source admission failure"]
pub(super) struct ParentLaunchCustody {
    unit: String,
    nonce: String,
    original_deadline: Instant,
    source_launcher: owner::LauncherLease,
    initial: owner::ManagerQuery,
    recheck: owner::ManagerQuery,
    snapshots: [Option<owner::ManagerSnapshot>; 2],
    // Completed pre-start query/wait/output evidence remains owned. Its actual
    // three descriptions are explicitly retired before the next query starts;
    // these non-admitted observations never become a live identity.
    readiness: Vec<(owner::ManagerQuery, owner::ManagerSnapshot)>,
    // Descriptive startup identity only; native custody is still absent.
    starting_identity: Option<NativeIdentity>,
    identity: Option<NativeIdentity>,
    pidfd: Option<OwnedFd>,
    directory: Option<OwnedFd>,
    directory_identity: Option<owner::FileIdentity>,
    stage: Stage,
    refused: Option<Failure>,
    failure_origin: Option<u64>,
    retirement_deadline: Option<Instant>,
}
impl ParentLaunchCustody {
    /// Borrowed descriptor numbers for explicit owner-controlled retirement;
    /// no handle is created, released or certified terminal here.
    pub(super) fn held_descriptors(&self) -> Vec<i32> {
        let mut fds = vec![self.source_launcher.held_descriptor()];
        if let Some(fd) = &self.pidfd {
            fds.push(fd.as_raw_fd());
        }
        if let Some(fd) = &self.directory {
            fds.push(fd.as_raw_fd());
        }
        fds.extend(self.initial.held_descriptors());
        fds.extend(self.recheck.held_descriptors());
        for (query, _) in &self.readiness {
            fds.extend(query.held_descriptors());
        }
        fds
    }
    pub(super) fn captured_native_inputs(&self) -> io::Result<CapturedNativeInputs<'_>> {
        require(
            self.stage == Stage::Captured,
            "native inputs require original completed parent capture",
        )?;
        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| io::Error::other("original parent identity absent"))?;
        let pidfd = self
            .pidfd
            .as_ref()
            .ok_or_else(|| io::Error::other("original parent pidfd absent"))?;
        let directory = self
            .directory
            .as_ref()
            .ok_or_else(|| io::Error::other("original parent cgroup handle absent"))?;
        let held = self
            .directory_identity
            .as_ref()
            .ok_or_else(|| io::Error::other("original parent cgroup identity absent"))?;
        let first = self.snapshots[0]
            .as_ref()
            .ok_or_else(|| io::Error::other("original initial manager snapshot absent"))?;
        let second = self.snapshots[1]
            .as_ref()
            .ok_or_else(|| io::Error::other("original recheck manager snapshot absent"))?;
        require(
            owner::filesystem(pidfd.as_raw_fd())? == 0x5049_4446
                && owner::filesystem(directory.as_raw_fd())? == 0x6367_7270
                && owner::stat(directory.as_raw_fd())?.same_object(held),
            "captured native descriptor identity changed",
        )?;
        for snapshot in [first, second] {
            require(
                snapshot.unit() == self.unit
                    && snapshot.property("InvocationID") == Some(identity.invocation.as_str()),
                "captured manager identity changed",
            )?;
        }
        Ok(CapturedNativeInputs {
            unit: &self.unit,
            nonce: &self.nonce,
            pid: identity.pid,
            invocation: &identity.invocation,
            cgroup: &identity.cgroup,
            pidfd: pidfd.as_fd(),
            directory: directory.as_fd(),
            directory_identity: held,
            snapshots: [first, second],
            launcher: &self.source_launcher,
            original_deadline: self.original_deadline,
            first_failure: self.refused.as_ref(),
            failure_origin: self.failure_origin,
        })
    }
    /// Infallible transfer of the actual Source Launcher's duplicate lease.
    /// The trusted launch caller binds that launcher to this exact unit. This
    /// object does not authenticate an arbitrary wrapper from a supplied PID.
    pub fn retain(
        unit: String,
        nonce: String,
        original_deadline: Instant,
        source_launcher: owner::LauncherLease,
    ) -> Self {
        Self {
            initial: owner::ManagerQuery::retain(unit.clone()),
            recheck: owner::ManagerQuery::retain(unit.clone()),
            unit,
            nonce,
            original_deadline,
            source_launcher,
            snapshots: [None, None],
            readiness: Vec::new(),
            starting_identity: None,
            identity: None,
            pidfd: None,
            directory: None,
            directory_identity: None,
            stage: Stage::Retained,
            refused: None,
            failure_origin: None,
            retirement_deadline: None,
        }
    }
    fn check(&self) -> io::Result<()> {
        if let Some(error) = &self.refused {
            return Err(error.error());
        }
        require(
            Instant::now() < self.original_deadline,
            "parent launch original stage deadline expired",
        )?;
        self.source_launcher.check_live()
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            if self.refused.is_none() {
                self.refused = Some(Failure::capture(error));
                // Unknown origin remains unknown; a later call cannot replace it.
                self.failure_origin = super::guardian::monotonic_ns().ok();
            }
        }
        result
    }
    pub fn start(&mut self) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.stage == Stage::Retained,
                "parent launch capture cannot restart",
            )?;
            owner::protected_holder()?;
            require(
                super::valid_nonce(&self.nonce)
                    && self.unit == format!("hermit-accepted-{}.service", self.nonce),
                "parent launch unit differs from original fresh nonce",
            )?;
            require(
                self.original_deadline
                    .saturating_duration_since(Instant::now())
                    <= std::time::Duration::from_secs(20),
                "parent launch original20s bound extended",
            )?;
            self.stage = Stage::InitialQuery;
            self.initial.start()
        })();
        self.remember(result)
    }
    fn pre_start_snapshot(&self, snapshot: &owner::ManagerSnapshot) -> bool {
        // This exact native startup state has no InvocationID or process yet.
        // It can only request another bounded observation, never admission.
        snapshot.unit() == self.unit
            && snapshot.property("Id") == Some(self.unit.as_str())
            && [
                ("LoadState", "loaded"),
                ("ActiveState", "inactive"),
                ("SubState", "dead"),
                ("InvocationID", ""),
                ("MainPID", "0"),
                ("ExecMainPID", "0"),
                ("ExecMainCode", "0"),
                ("ExecMainStatus", "0"),
                ("Result", "success"),
                ("ControlGroup", ""),
                ("TasksCurrent", "[not set]"),
            ]
            .into_iter()
            .all(|(key, value)| snapshot.property(key) == Some(value))
    }
    fn pending_start_identity(
        &self,
        snapshot: &owner::ManagerSnapshot,
    ) -> io::Result<Option<NativeIdentity>> {
        if snapshot.property("ActiveState") != Some("activating")
            || snapshot.property("SubState") != Some("start")
        {
            return Ok(None);
        }
        require(
            snapshot.unit() == self.unit
                && snapshot.property("Id") == Some(self.unit.as_str())
                && [
                    ("LoadState", "loaded"),
                    ("ExecMainCode", "0"),
                    ("ExecMainStatus", "0"),
                    ("Result", "success"),
                    ("TasksCurrent", "1"),
                ]
                .into_iter()
                .all(|(key, value)| snapshot.property(key) == Some(value)),
            "parent launch starting snapshot differs from original unit/start state",
        )?;
        let invocation = snapshot
            .property("InvocationID")
            .ok_or_else(|| io::Error::other("parent launch starting invocation absent"))?;
        require(
            super::valid_nonce(invocation),
            "parent launch starting invocation missing or malformed",
        )?;
        let text = snapshot
            .property("MainPID")
            .ok_or_else(|| io::Error::other("parent launch starting PID absent"))?;
        let pid: libc::pid_t = text.parse().map_err(io::Error::other)?;
        require(
            pid > 0 && pid.to_string() == text && snapshot.property("ExecMainPID") == Some(text),
            "parent launch starting main/exec PID differs",
        )?;
        let cgroup = snapshot
            .property("ControlGroup")
            .ok_or_else(|| io::Error::other("parent launch starting cgroup absent"))?;
        require(
            cgroup.starts_with('/')
                && cgroup != "/"
                && cgroup.split('/').all(|part| !matches!(part, "." | "..")),
            "parent launch starting cgroup malformed",
        )?;
        Ok(Some(NativeIdentity {
            pid,
            invocation: invocation.to_owned(),
            cgroup: cgroup.to_owned(),
        }))
    }
    fn readiness_evidence(&self) -> Vec<serde_json::Value> {
        self.readiness
            .iter()
            .map(|(query, snapshot)| {
                json!({
            "query":query.evidence(),"snapshot":snapshot.evidence(),"admitted":false})
            })
            .collect()
    }
    fn live_identity(&self, snapshot: &owner::ManagerSnapshot) -> io::Result<NativeIdentity> {
        require(
            snapshot.unit() == self.unit
                && snapshot.property("Id") == Some(self.unit.as_str())
                && snapshot.property("LoadState") == Some("loaded"),
            "parent launch manager unit is not the original loaded unit",
        )?;
        let invocation = snapshot
            .property("InvocationID")
            .ok_or_else(|| io::Error::other("parent launch manager invocation absent"))?;
        require(
            super::valid_nonce(invocation),
            "parent launch manager invocation missing or malformed",
        )?;
        require(
            snapshot.property("ActiveState") == Some("active")
                && snapshot.property("SubState") == Some("running")
                && snapshot.property("ExecMainCode") == Some("0")
                && snapshot.property("ExecMainStatus") == Some("0"),
            "parent launch manager creator is not actively running",
        )?;
        let text = snapshot
            .property("MainPID")
            .ok_or_else(|| io::Error::other("parent launch manager main PID absent"))?;
        let pid: libc::pid_t = text.parse().map_err(io::Error::other)?;
        require(
            pid > 0 && pid.to_string() == text && snapshot.property("ExecMainPID") == Some(text),
            "parent launch manager main/exec PID differs",
        )?;
        let cgroup = snapshot
            .property("ControlGroup")
            .ok_or_else(|| io::Error::other("parent launch manager cgroup absent"))?;
        require(
            cgroup.starts_with('/')
                && cgroup != "/"
                && cgroup.split('/').all(|part| !matches!(part, "." | "..")),
            "parent launch manager cgroup malformed",
        )?;
        Ok(NativeIdentity {
            pid,
            invocation: invocation.to_owned(),
            cgroup: cgroup.to_owned(),
        })
    }
    fn acquire_native_handles(&mut self) -> io::Result<()> {
        require(
            self.pidfd.is_none() && self.directory.is_none() && self.directory_identity.is_none(),
            "parent launch native handles cannot be recaptured",
        )?;
        let identity = self.identity.as_ref().unwrap();
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        self.pidfd = Some(unsafe { OwnedFd::from_raw_fd(raw as i32) });
        require(
            owner::filesystem(self.pidfd.as_ref().unwrap().as_raw_fd())? == 0x5049_4446
                && unsafe { libc::fcntl(self.pidfd.as_ref().unwrap().as_raw_fd(), libc::F_GETFD) }
                    == libc::FD_CLOEXEC,
            "parent launch original pidfd type or CLOEXEC differs",
        )?;
        owner::pidfd_matches(self.pidfd.as_ref().unwrap().as_raw_fd(), identity.pid)?;
        let text = owner::read_file(&format!("/proc/{}/cgroup", identity.pid), 4096)?;
        require(
            text == format!("0::{}\n", identity.cgroup),
            "parent launch native membership differs from manager",
        )?;
        let path =
            CString::new(format!("/sys/fs/cgroup{}", identity.cgroup)).map_err(io::Error::other)?;
        let raw = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        self.directory = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        self.directory_identity = Some(owner::stat(raw)?);
        self.check_native_live()
    }
    fn check_native_live(&self) -> io::Result<()> {
        self.check()?;
        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| io::Error::other("parent launch identity absent"))?;
        let pidfd = self
            .pidfd
            .as_ref()
            .ok_or_else(|| io::Error::other("parent launch pidfd absent"))?
            .as_raw_fd();
        let directory = self
            .directory
            .as_ref()
            .ok_or_else(|| io::Error::other("parent launch cgroup directory absent"))?
            .as_raw_fd();
        let held = self
            .directory_identity
            .as_ref()
            .ok_or_else(|| io::Error::other("parent launch cgroup identity absent"))?;
        owner::pidfd_matches(pidfd, identity.pid)?;
        require(
            owner::filesystem(directory)? == 0x6367_7270,
            "parent launch directory is not cgroup2",
        )?;
        let flags = unsafe { libc::fcntl(directory, libc::F_GETFL) };
        require(
            flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDONLY,
            "parent launch cgroup is not read-only",
        )?;
        require(
            unsafe { libc::fcntl(directory, libc::F_GETFD) } == libc::FD_CLOEXEC,
            "parent launch cgroup lacks CLOEXEC",
        )?;
        let path =
            CString::new(format!("/sys/fs/cgroup{}", identity.cgroup)).map_err(io::Error::other)?;
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::lstat(path.as_ptr(), named.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let actual = owner::FileIdentity::from(unsafe { named.assume_init() });
        require(
            held.mode & libc::S_IFMT == libc::S_IFDIR
                && held.same_object(&actual)
                && owner::stat(directory)?.same_object(held),
            "parent launch cgroup path/held identity changed",
        )?;
        require(
            owner::read_file(&format!("/proc/{}/cgroup", identity.pid), 4096)?
                == format!("0::{}\n", identity.cgroup),
            "parent launch creator changed native membership",
        )?;
        let procs = owner::read_at(directory, "cgroup.procs")?;
        require(
            procs.lines().any(|p| p == identity.pid.to_string()),
            "parent launch creator absent from retained cgroup",
        )?;
        for (name, expected) in [
            ("memory.max", "268435456\n"),
            ("memory.swap.max", "0\n"),
            ("pids.max", "8\n"),
            ("cpu.max", "100000 100000\n"),
        ] {
            require(
                owner::read_at(directory, name)? == expected,
                "parent launch original cgroup bound differs",
            )?;
        }
        let peer = wire::Credentials {
            pid: identity.pid,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        };
        owner::validate_process_status(
            &owner::read_file(&format!("/proc/{}/status", identity.pid), 16384)?,
            peer,
        )?;
        for (name, resource, expected) in [
            (
                "NOFILE",
                libc::RLIMIT_NOFILE,
                super::super::capability_unit::CAPABILITY_UNIT_NOFILE,
            ),
            ("FSIZE", libc::RLIMIT_FSIZE, 1048576),
            ("CORE", libc::RLIMIT_CORE, 0),
        ] {
            let mut limits = std::mem::MaybeUninit::<libc::rlimit>::uninit();
            if unsafe {
                libc::prlimit(
                    identity.pid,
                    resource,
                    std::ptr::null(),
                    limits.as_mut_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            let limits = unsafe { limits.assume_init() };
            if limits.rlim_cur != expected || limits.rlim_max != expected {
                return Err(io::Error::other(format!(
                    "parent launch original process bound differs: pid {} RLIMIT_{name} soft {} hard {}, expected {expected}",
                    identity.pid, limits.rlim_cur, limits.rlim_max
                )));
            }
        }
        owner::pidfd_matches(pidfd, identity.pid)?;
        self.check()
    }
    pub fn progress(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            match self.stage {
                Stage::InitialQuery => {
                    let Some(snapshot) = self.initial.poll(self.original_deadline)? else {
                        return Ok(false);
                    };
                    self.snapshots[0] = Some(snapshot);
                    let pre_start = self.pre_start_snapshot(self.snapshots[0].as_ref().unwrap());
                    let starting =
                        self.pending_start_identity(self.snapshots[0].as_ref().unwrap())?;
                    if pre_start || starting.is_some() {
                        self.check()?;
                        require(
                            self.identity.is_none()
                                && self.pidfd.is_none()
                                && self.directory.is_none()
                                && self.snapshots[1].is_none(),
                            "pre-start readiness cannot replace captured identity",
                        )?;
                        if pre_start {
                            require(
                                self.starting_identity.is_none(),
                                "parent launch regressed after original starting identity",
                            )?;
                        } else if let Some(starting) = starting {
                            if let Some(original) = &self.starting_identity {
                                require(
                                    *original == starting,
                                    "parent launch starting identity replaced while pending",
                                )?;
                            } else {
                                self.starting_identity = Some(starting);
                            }
                        }
                        // Count this completed query, its successor and the
                        // mandatory recheck in the existing64 command bound.
                        require(
                            self.readiness.len() + 3 <= 64,
                            "parent launch readiness query count bound",
                        )?;
                        self.initial
                            .retire_successful_resources(self.original_deadline)?;
                        require(
                            self.initial.successful_resources_retired()?,
                            "completed readiness query resources remain held",
                        )?;
                        let completed = std::mem::replace(
                            &mut self.initial,
                            owner::ManagerQuery::retain(self.unit.clone()),
                        );
                        self.readiness
                            .push((completed, self.snapshots[0].take().unwrap()));
                        // The new physical query keeps its own original2s;
                        // every poll still intersects the unchanged parent cut.
                        self.initial.start()?;
                        self.check()?;
                        return Ok(false);
                    }
                    let live = self.live_identity(self.snapshots[0].as_ref().unwrap())?;
                    if let Some(starting) = &self.starting_identity {
                        require(
                            *starting == live,
                            "parent launch live identity differs from original starting observation",
                        )?;
                    }
                    self.identity = Some(live);
                    self.acquire_native_handles()?;
                    self.stage = Stage::RecheckQuery;
                    self.recheck.start()?;
                    Ok(false)
                }
                Stage::RecheckQuery => {
                    let Some(snapshot) = self.recheck.poll(self.original_deadline)? else {
                        return Ok(false);
                    };
                    self.snapshots[1] = Some(snapshot);
                    let observed = self.live_identity(self.snapshots[1].as_ref().unwrap())?;
                    require(
                        self.identity.as_ref() == Some(&observed),
                        "parent launch manager identity replaced during capture",
                    )?;
                    self.check_native_live()?;
                    self.stage = Stage::Captured;
                    Ok(true)
                }
                Stage::Captured => {
                    self.check_native_live()?;
                    Ok(true)
                }
                Stage::Retained => Err(io::Error::other("parent launch capture was not started")),
            }
        })();
        self.remember(result)
    }
    /// Data readback only, never a constructor for Creator/admission/deletion.
    pub fn captured_observation(&mut self) -> io::Result<serde_json::Value> {
        let result = (|| {
            self.check()?;
            require(
                self.stage == Stage::Captured,
                "parent launch custody was not captured",
            )?;
            self.check_native_live()?;
            let identity = self.identity.as_ref().unwrap();
            let directory = self.directory_identity.as_ref().unwrap();
            Ok(
                json!({"kind":"original-parent-launch-custody-v1","unit":self.unit,"nonce":self.nonce,"pid":identity.pid,
            "invocation":identity.invocation,"cgroup":identity.cgroup,"device":directory.device,"inode":directory.inode,
            "original_creator_pidfd_held":true,"creator_pidfd":self.pidfd.as_ref().unwrap().as_raw_fd(),
            "original_readonly_cgroup_held":true,"cgroup_fd":self.directory.as_ref().unwrap().as_raw_fd(),"source_launcher_original_lease_held":true,
            "initial_manager":self.snapshots[0].as_ref().unwrap().evidence(),"rechecked_manager":self.snapshots[1].as_ref().unwrap().evidence(),
            "initial_query":self.initial.evidence(),"recheck_query":self.recheck.evidence(),
            "pre_start_observations":self.readiness_evidence(),"creator_admission_issued":false,
            "source_terminal_issued":false,"provider_authority_issued":false}),
            )
        })();
        self.remember(result)
    }
    /// Export borrowed descriptions only from this original completed capture.
    /// SCM duplicates do not transfer this owner's query/Child wait authority.
    /// The receiver independently authenticates its Creator against both these
    /// held objects and actual manager/image observations before any adoption.
    pub(super) fn captured_transfer(
        &mut self,
        run: &str,
        native_deadline: u64,
        arguments: &[String],
    ) -> io::Result<(serde_json::Value, [BorrowedFd<'_>; 2])> {
        self.check()?;
        require(
            self.stage == Stage::Captured
                && super::valid_nonce(run)
                && super::guardian::monotonic_ns()? < native_deadline,
            "provider transfer lacks original live captured stage",
        )?;
        self.check_native_live()?;
        let identity = self.identity.as_ref().unwrap();
        let directory = self.directory_identity.as_ref().unwrap();
        let record = json!({"schema":"hermit-grouped-provider-capture-v1",
            "run":run,"stage_deadline":native_deadline,"unit":self.unit,
            "invocation":identity.invocation,"pid":identity.pid,"cgroup":identity.cgroup,
            "device":directory.device,"inode":directory.inode,"argv":arguments});
        Ok((
            record,
            [
                self.pidfd.as_ref().unwrap().as_fd(),
                self.directory.as_ref().unwrap().as_fd(),
            ],
        ))
    }

    /// A refused capture still owns both current query objects, every completed
    /// pre-start query/snapshot, and any partially
    /// acquired native handles. Do not parse a cleanup result as a Snapshot.
    pub fn retire_queries(&mut self, deadline: Instant) -> io::Result<bool> {
        let cause = self
            .refused
            .as_ref()
            .ok_or_else(|| io::Error::other("parent launch query retirement precedes refusal"))?
            .error();
        let deadline = self.failure_deadline(deadline)?;
        // Historical readiness observations retain their actual successful wait,
        // output and one-use close receipts. They cannot reconstruct a fresh
        // CompletedQuery or enter failed-query retirement after resource release.
        for (query, _) in &self.readiness {
            require(
                query.successful_resources_retired()?,
                "readiness resource retirement absent",
            )?;
        }
        let initial = if self.snapshots[0].is_some() {
            // false means the active successful query still owns its admission
            // handles; an incomplete attempted release is an explicit error.
            self.initial.successful_resources_retired()?;
            None
        } else {
            Some(self.initial.retire_custody(deadline, &cause)?)
        };
        let recheck = if self.snapshots[1].is_some() {
            self.recheck.successful_resources_retired()?;
            None
        } else {
            Some(self.recheck.retire_custody(deadline, &cause)?)
        };
        Ok(!matches!(initial, Some(owner::QueryRetirement::Pending))
            && !matches!(recheck, Some(owner::QueryRetirement::Pending)))
    }
    pub fn failure_deadline(&mut self, caller: Instant) -> io::Result<Instant> {
        require(
            self.refused.is_some(),
            "parent launch first-failure cutoff precedes refusal",
        )?;
        let origin = self
            .failure_origin
            .ok_or_else(|| io::Error::other("parent launch original failure origin unknown"))?;
        let sampled = Instant::now();
        let now = super::guardian::monotonic_ns()?;
        require(
            now >= origin && now - origin < 1_000_000_000,
            "parent launch original first-failure1s expired or future",
        )?;
        let bound = (sampled + std::time::Duration::from_nanos(1_000_000_000 - (now - origin)))
            .min(self.original_deadline)
            .min(caller);
        let fixed = self.retirement_deadline.map_or(bound, |old| old.min(bound));
        self.retirement_deadline = Some(fixed);
        require(
            Instant::now() < fixed,
            "parent launch original retirement deadline expired",
        )?;
        Ok(fixed)
    }
    pub fn diagnostics(&self) -> serde_json::Value {
        json!({"unit":self.unit,"nonce":self.nonce,"stage":format!("{:?}",self.stage),"pidfd_held":self.pidfd.is_some(),
            "directory_held":self.directory.is_some(),"native_identity_captured":self.stage==Stage::Captured&&self.refused.is_none(),
            "first_failure":self.refused.as_ref().map(|f|&f.message),"failure_origin":self.failure_origin,
            "original_query_snapshots_owned":self.snapshots.each_ref().map(Option::is_some),
            "initial_snapshot":self.snapshots[0].as_ref().map(owner::ManagerSnapshot::evidence),
            "recheck_snapshot":self.snapshots[1].as_ref().map(owner::ManagerSnapshot::evidence),
            "initial_query":self.initial.evidence(),"recheck_query":self.recheck.evidence(),
            "pre_start_observations":self.readiness_evidence(),"query_count_bound":64,
            "starting_identity_observation":self.starting_identity.as_ref().map(|identity|json!({
                "invocation":identity.invocation,"pid":identity.pid,"cgroup":identity.cgroup,"native_custody":false}))})
    }
}

#[path = "parent_retirement.rs"]
mod retirement;
pub(super) use retirement::ParentFailedRetirement;
pub(super) use retirement::ParentFailedUnitProof;
