//! Outside Keeper source-launch custody. Neither transported descriptions nor
//! diagnostics construct a Child, Creator, or successful SourceTerminal.
use std::time::Duration;

use serde_json::Value;
use serde_json::json;

use super::super::guardian;
use super::super::journal;
use super::super::parent_launch;
use super::super::wire;
use super::*;

/// Non-wait lease of the actual Child which remains owned by the independently
/// retained ordinary source owner. Admission brackets native PPid/PGID with the held
/// live pidfds and the exact authenticated original launch reply.
#[derive(Debug)]
pub(in super::super) struct RemoteLauncherLease {
    lease: LauncherLease,
    runtime: OwnedFd,
    wait_owner: OwnedFd,
    wait_owner_pid: i32,
    peer: wire::Credentials,
    admitted: bool,
}
impl RemoteLauncherLease {
    pub fn receive(
        slot: &mut Option<Self>,
        packet: &mut wire::Packet,
        runtime: BorrowedFd<'_>,
        peer: wire::Credentials,
        intent: &Intent,
        unit: &str,
        stage: u64,
    ) -> io::Result<()> {
        require(
            slot.is_none(),
            "remote source launcher cannot be recaptured",
        )?;
        packet.exact(2, peer)?;
        let value: Value = serde_json::from_slice(&packet.bytes)?;
        let pid = i32::try_from(
            value["wrapper_pid"]
                .as_i64()
                .ok_or_else(|| io::Error::other("remote wrapper PID absent"))?,
        )
        .map_err(io::Error::other)?;
        let wait_owner_pid = value["wait_owner_pid"]
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .filter(|v| *v > 1 && *v != peer.pid)
            .ok_or_else(|| io::Error::other("actual remote wait-owner PID absent"))?;
        require(
            pid > 0
                && packet.bytes
                    == journal::canonical(
                        &json!({"schema":"hermit-grouped-runtime-source-launched-v1",
            "nonce":intent.nonce,"incarnation":intent.incarnation,"stage_deadline":stage,"unit":unit,"wrapper_pid":pid,"wait_owner_pid":wait_owner_pid}),
                    )?,
            "remote source launch reply differs from original request",
        )?;
        let runtime = runtime.try_clone_to_owned()?;
        let wrapper = packet.rights.remove(0);
        let wait_owner = packet.rights.remove(0);
        *slot = Some(Self {
            lease: LauncherLease {
                pid,
                pidfd: wrapper,
            },
            runtime,
            wait_owner,
            wait_owner_pid,
            peer,
            admitted: false,
        });
        let owner = slot.as_mut().unwrap();
        owner.validate_live()?;
        owner.admitted = true;
        Ok(())
    }
    fn validate_live(&self) -> io::Result<()> {
        pidfd_matches(self.runtime.as_raw_fd(), self.peer.pid)?;
        require(
            !terminal(self.runtime.as_raw_fd())?,
            "original outside runtime owner is terminal",
        )?;
        pidfd_matches(self.lease.pidfd.as_raw_fd(), self.lease.pid)?;
        require(
            !terminal(self.lease.pidfd.as_raw_fd())?,
            "remote original source wrapper already terminal",
        )?;
        pidfd_matches(self.wait_owner.as_raw_fd(), self.wait_owner_pid)?;
        require(
            !terminal(self.wait_owner.as_raw_fd())?,
            "actual original source wait owner is terminal",
        )?;
        let text = read_file(&format!("/proc/{}/status", self.lease.pid), 16384)?;
        let parents = text
            .lines()
            .filter_map(|line| line.strip_prefix("PPid:"))
            .map(str::trim)
            .collect::<Vec<_>>();
        require(
            parents == [self.wait_owner_pid.to_string()],
            "source wrapper is not an actual child of its retained source owner",
        )?;
        self.lease.check_live()?;
        pidfd_matches(self.runtime.as_raw_fd(), self.peer.pid)?;
        pidfd_matches(self.wait_owner.as_raw_fd(), self.wait_owner_pid)?;
        require(
            !terminal(self.wait_owner.as_raw_fd())?,
            "original source wait owner died during admission",
        )?;
        pidfd_matches(self.lease.pidfd.as_raw_fd(), self.lease.pid)
    }
    pub fn pid(&self) -> i32 {
        self.lease.pid
    }
    pub fn pidfd(&self) -> BorrowedFd<'_> {
        self.lease.pidfd.as_fd()
    }
    pub fn lend(&self) -> io::Result<LauncherLease> {
        require(self.admitted, "remote source launcher was never admitted")?;
        self.validate_live()?;
        Ok(LauncherLease {
            pid: self.lease.pid,
            pidfd: self.lease.pidfd.try_clone()?,
        })
    }
    pub fn check_terminal(&self) -> io::Result<()> {
        require(
            self.admitted && terminal(self.lease.pidfd.as_raw_fd())?,
            "original remote source wrapper is not terminal",
        )?;
        pidfd_matches(self.runtime.as_raw_fd(), self.peer.pid)?;
        require(
            !terminal(self.runtime.as_raw_fd())?,
            "actual remote wait owner is terminal",
        )?;
        pidfd_matches(self.wait_owner.as_raw_fd(), self.wait_owner_pid)?;
        require(
            !terminal(self.wait_owner.as_raw_fd())?,
            "actual source wait owner exited before controller terminal admission",
        )?;
        let raw = unsafe { libc::kill(-self.lease.pid, 0) };
        let error = (raw == -1).then(io::Error::last_os_error);
        require(
            raw == -1 && error.as_ref().and_then(io::Error::raw_os_error) == Some(libc::ESRCH),
            "original remote source group is not positively absent",
        )
    }
    pub fn held_descriptors(&self) -> Vec<RawFd> {
        vec![
            self.lease.pidfd.as_raw_fd(),
            self.runtime.as_raw_fd(),
            self.wait_owner.as_raw_fd(),
        ]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Binding {
    unit: String,
    invocation: String,
    pid: i32,
    wrapper: i32,
    device: u64,
    inode: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Stop,
    Forget,
}
/// Only the real outside parent uses this after actual controller death. It
/// borrows its retained ParentLaunchCustody and real Launcher on every call;
/// no supplied terminal JSON or reconstructed Creator can authorize a command.
#[derive(Debug)]
pub(in super::super) struct OutsideSourceRetirement {
    controller: OwnedFd,
    stage: u64,
    origin: Option<u64>,
    cutoff: Option<u64>,
    deadline: Option<Instant>,
    binding: Option<Binding>,
    terminal_query: Option<ManagerQuery>,
    terminal_snapshot: Option<ManagerSnapshot>,
    command: Option<CommandQuery>,
    action: Option<Action>,
    command_attempted: bool,
    command_done: bool,
    absence_query: Option<ManagerQuery>,
    absence_snapshot: Option<ManagerSnapshot>,
    complete: bool,
    failure: Option<Failure>,
    normal_source_already_joined: Option<bool>,
    unit_retired: bool,
    normal_stop_wait: Option<(i32, i32, i32)>,
}
impl OutsideSourceRetirement {
    pub fn retain(controller: OwnedFd, stage: u64) -> Self {
        Self {
            controller,
            stage,
            origin: None,
            cutoff: None,
            deadline: None,
            binding: None,
            terminal_query: None,
            terminal_snapshot: None,
            command: None,
            action: None,
            command_attempted: false,
            command_done: false,
            absence_query: None,
            absence_snapshot: None,
            complete: false,
            failure: None,
            normal_source_already_joined: None,
            unit_retired: false,
            normal_stop_wait: None,
        }
    }
    fn bound(
        &mut self,
        inputs: &parent_launch::CapturedNativeInputs<'_>,
        origin: u64,
        enclosing: u64,
    ) -> io::Result<Instant> {
        let now = guardian::monotonic_ns()?;
        if let Some(prior) = self.origin {
            require(
                prior == origin,
                "outside source first failure origin changed",
            )?;
        } else {
            self.origin = Some(origin);
        }
        let earliest =
            if inputs.first_failure.is_some() {
                origin.min(inputs.failure_origin.ok_or_else(|| {
                    io::Error::other("original source parent failure origin unknown")
                })?)
            } else {
                origin
            };
        let cutoff = earliest
            .checked_add(1_000_000_000)
            .ok_or_else(|| io::Error::other("source retirement cutoff overflow"))?
            .min(enclosing)
            .min(self.stage);
        self.cutoff = Some(self.cutoff.map_or(cutoff, |old| old.min(cutoff)));
        let cutoff = self.cutoff.unwrap();
        require(
            now >= origin && now >= earliest && now < cutoff,
            "outside source original first failure1s expired or future",
        )?;
        let deadline =
            (Instant::now() + Duration::from_nanos(cutoff - now)).min(inputs.original_deadline);
        self.deadline = Some(self.deadline.map_or(deadline, |old| old.min(deadline)));
        self.check()
    }
    fn check(&self) -> io::Result<Instant> {
        require(
            terminal(self.controller.as_raw_fd())?,
            "source unit takeover precedes actual controller terminality",
        )?;
        let deadline = self
            .deadline
            .ok_or_else(|| io::Error::other("source retirement has no original cutoff"))?;
        require(
            Instant::now() < deadline && guardian::monotonic_ns()? < self.cutoff.unwrap(),
            "outside source original retirement cutoff expired",
        )?;
        Ok(deadline)
    }
    fn bind(
        &mut self,
        inputs: &parent_launch::CapturedNativeInputs<'_>,
        source: &Launcher,
    ) -> io::Result<()> {
        require(
            inputs.launcher.pid == source.child.id() as i32,
            "outside source parent capture belongs to a different original Child",
        )?;
        let original = source
            .pidfd
            .as_ref()
            .ok_or_else(|| io::Error::other("actual source Launcher has no retained pidfd"))?;
        require(
            filesystem(original.as_raw_fd())? == 0x5049_4446,
            "outside source original Launcher descriptor is not pidfs",
        )?;
        let same = unsafe {
            libc::syscall(
                libc::SYS_kcmp,
                libc::getpid(),
                libc::getpid(),
                0,
                original.as_raw_fd() as libc::c_ulong,
                inputs.launcher.pidfd.as_raw_fd() as libc::c_ulong,
            )
        };
        if same < 0 {
            return Err(io::Error::last_os_error());
        }
        require(
            same == 0,
            "outside source launcher capture is not the original duplicate description",
        )?;
        let binding = Binding {
            unit: inputs.unit.to_owned(),
            invocation: inputs.invocation.to_owned(),
            pid: inputs.pid,
            wrapper: source.child.id() as i32,
            device: inputs.directory_identity.device,
            inode: inputs.directory_identity.inode,
        };
        if let Some(previous) = &self.binding {
            require(
                *previous == binding,
                "outside source original binding changed",
            )?;
        } else {
            self.binding = Some(binding);
        }
        Ok(())
    }
    fn terminal_identity(
        inputs: &parent_launch::CapturedNativeInputs<'_>,
        snapshot: &ManagerSnapshot,
    ) -> io::Result<()> {
        let pid = inputs.pid.to_string();
        require(
            snapshot.unit() == inputs.unit
                && snapshot.property("Id") == Some(inputs.unit)
                && snapshot.property("LoadState") == Some("loaded")
                && snapshot.property("InvocationID") == Some(inputs.invocation)
                && snapshot.property("ExecMainPID") == Some(pid.as_str())
                && snapshot.property("MainPID") == Some("0")
                && snapshot
                    .property("ControlGroup")
                    .is_some_and(|g| g.is_empty() || g == inputs.cgroup),
            "outside terminal manager changed captured unit, creator or invocation",
        )
    }
    fn absent(
        inputs: &parent_launch::CapturedNativeInputs<'_>,
        snapshot: &ManagerSnapshot,
    ) -> io::Result<()> {
        require(
            snapshot.unit() == inputs.unit
                && snapshot.property("Id") == Some(inputs.unit)
                && snapshot.property("LoadState") == Some("not-found")
                && snapshot.property("InvocationID") == Some("")
                && snapshot.property("MainPID") == Some("0")
                && snapshot.property("ControlGroup") == Some(""),
            "outside source unit is not actually absent",
        )
    }
    pub fn progress(
        &mut self,
        parent: &parent_launch::ParentLaunchCustody,
        source: &mut Launcher,
        logs: BorrowedFd<'_>,
        ledger: &mut journal::Store,
        origin: u64,
        enclosing: u64,
    ) -> io::Result<bool> {
        let result = self.progress_inner(parent, source, logs, ledger, origin, enclosing);
        if let Err(error) = &result {
            self.failure.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn progress_inner(
        &mut self,
        parent: &parent_launch::ParentLaunchCustody,
        source: &mut Launcher,
        logs: BorrowedFd<'_>,
        ledger: &mut journal::Store,
        origin: u64,
        enclosing: u64,
    ) -> io::Result<bool> {
        if let Some(failure) = &self.failure {
            return Err(failure.error());
        }
        let inputs = parent.captured_native_inputs()?;
        let deadline = self.bound(&inputs, origin, enclosing)?;
        self.bind(&inputs, source)?;
        ledger.verify()?;
        if self.complete {
            return Ok(true);
        }
        let already_joined = *self
            .normal_source_already_joined
            .get_or_insert(source.reaped.is_some());
        if !terminal(inputs.pidfd.as_raw_fd())? {
            return Ok(false);
        }
        let native =
            read_retained_cgroup(inputs.pidfd, inputs.directory, inputs.directory_identity)?;
        if native.terminal_progress()? == TerminalProgress::Pending {
            return Ok(false);
        }
        if self.terminal_query.is_none() {
            self.terminal_query = Some(ManagerQuery::retain(inputs.unit.to_owned()));
            self.terminal_query.as_mut().unwrap().start()?;
        }
        if self.terminal_snapshot.is_none() {
            self.terminal_snapshot = self.terminal_query.as_mut().unwrap().poll(deadline)?;
        }
        let Some(snapshot) = &self.terminal_snapshot else {
            return Ok(false);
        };
        if already_joined {
            // A later S2 failure must preserve the actual successful source
            // Child already joined by the normal terminal sender. It cannot
            // be called failed or waited a second time.
            require(
                source.reaped.is_some_and(|s| s.code() == Some(0))
                    && source.eof == [true, true]
                    && source.logs_synced,
                "previous source wait is not the retained complete normal success",
            )?;
            Self::absent(&inputs, snapshot)?;
            let actual =
                read_retained_cgroup(inputs.pidfd, inputs.directory, inputs.directory_identity)?;
            require(
                matches!(actual,CgroupReadbackProgress::Observed(v) if v.creator_terminal&&v.unlinked),
                "previous successful source still has a linked native cgroup",
            )?;
        } else if snapshot.property("LoadState") == Some("not-found") {
            // The original controller may finish stop before dying, while the
            // outside owner has not yet consumed its actual source Child. Keep
            // that successful WNOWAIT distinct from an earlier completed join.
            Self::absent(&inputs, snapshot)?;
            let actual =
                read_retained_cgroup(inputs.pidfd, inputs.directory, inputs.directory_identity)?;
            require(
                matches!(actual,CgroupReadbackProgress::Observed(v) if v.creator_terminal&&v.unlinked),
                "already-stopped source lacks original terminal unlinked cgroup",
            )?;
            if self.normal_stop_wait.is_none() {
                require(
                    source.reaped.is_none(),
                    "unjoined original successful wait was already consumed",
                )?;
                let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
                let raw = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        source.child.id(),
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                if raw == -1 {
                    return Err(io::Error::last_os_error());
                }
                require(
                    raw == 0,
                    "original successful WNOWAIT returned unexpected result",
                )?;
                let pid = unsafe { info.si_pid() };
                if pid == 0 {
                    return Ok(false);
                }
                let observed = (pid, info.si_code, unsafe { info.si_status() });
                self.normal_stop_wait = Some(observed);
                require(
                    observed == (source.child.id() as i32, libc::CLD_EXITED, 0),
                    "absent unit lacks original source natural-zero WNOWAIT",
                )?;
            }
            let cause =
                io::Error::other("original controller terminal after normal source unit stop");
            if source.retire_custody(logs.as_raw_fd(), deadline, &cause)?
                == QueryRetirement::Pending
            {
                return Ok(false);
            }
            require(
                source.reaped.is_some_and(|status| status.code() == Some(0)),
                "actual source natural-zero status changed during custody join",
            )?;
        } else {
            Self::terminal_identity(&inputs, snapshot)?;
            if self.action.is_none() {
                let action = if snapshot.property("ActiveState") == Some("active")
                    && snapshot.property("SubState") == Some("exited")
                    && snapshot.property("ExecMainCode") == Some("1")
                    && snapshot.property("ExecMainStatus") == Some("0")
                    && snapshot.property("Result") == Some("success")
                {
                    Action::Stop
                } else {
                    require(
                        snapshot.property("ActiveState") == Some("failed")
                            && snapshot.property("SubState") == Some("failed")
                            && snapshot.property("ExecMainCode") == Some("1")
                            && snapshot.property("Result") == Some("exit-code"),
                        "outside source terminal result is neither original success nor natural failure",
                    )?;
                    Action::Forget
                };
                self.action = Some(action);
                self.command = Some(CommandQuery::retain(vec![
                    "-n".to_owned(),
                    "/usr/bin/systemctl".to_owned(),
                    if action == Action::Stop {
                        "stop"
                    } else {
                        "reset-failed"
                    }
                    .to_owned(),
                    inputs.unit.to_owned(),
                ]));
            }
            if !self.command_attempted {
                match self.action.unwrap() {
                    Action::Stop => {
                        inputs.launcher.check_live()?;
                    }
                    Action::Forget => {
                        let text = snapshot
                            .property("ExecMainStatus")
                            .ok_or_else(|| io::Error::other("failed source status absent"))?;
                        let status: i32 = text.parse().map_err(io::Error::other)?;
                        require(
                            status > 0 && status <= 255 && status.to_string() == text,
                            "failed source status is not canonical",
                        )?;
                        let actual = read_retained_cgroup(
                            inputs.pidfd,
                            inputs.directory,
                            inputs.directory_identity,
                        )?;
                        if !matches!(actual,CgroupReadbackProgress::Observed(v) if v.creator_terminal&&v.unlinked)
                        {
                            return Ok(false);
                        }
                        inputs.launcher.check_failed_unreaped(status)?;
                    }
                }
                self.command_attempted = true;
                ledger.append(json!({"kind":"outside-original-source-unit-retirement-intent",
                    "unit":inputs.unit,"invocation":inputs.invocation,"source_pid":inputs.pid,
                    "wrapper_pid":source.child.id(),"origin":self.origin,"cutoff":self.cutoff,
                    "action":format!("{:?}",self.action.unwrap()),"terminal_manager":snapshot.evidence()}))?;
                self.check()?;
                self.command.as_mut().unwrap().start()?;
            }
            if !self.command_done {
                self.command_done = self.command.as_mut().unwrap().poll(deadline)?;
            }
            if !self.command_done {
                return Ok(false);
            }
            if self.absence_query.is_none() {
                self.absence_query = Some(ManagerQuery::retain(inputs.unit.to_owned()));
                self.absence_query.as_mut().unwrap().start()?;
            }
            if self.absence_snapshot.is_none() {
                self.absence_snapshot = self.absence_query.as_mut().unwrap().poll(deadline)?;
            }
            let Some(absent) = &self.absence_snapshot else {
                return Ok(false);
            };
            Self::absent(&inputs, absent)?;
            let actual =
                read_retained_cgroup(inputs.pidfd, inputs.directory, inputs.directory_identity)?;
            if !matches!(actual,CgroupReadbackProgress::Observed(v) if v.creator_terminal&&v.unlinked)
            {
                return Ok(false);
            }
            if !self.unit_retired {
                ledger.append(json!({"kind":"outside-original-source-unit-retired","manager":absent.evidence(),
                    "command":self.command.as_ref().unwrap().evidence(),"origin":self.origin,"cutoff":self.cutoff}))?;
                self.unit_retired = true;
            }
            let cause =
                io::Error::other("original startup controller terminal; source custody retirement");
            if source.retire_custody(logs.as_raw_fd(), deadline, &cause)?
                == QueryRetirement::Pending
            {
                return Ok(false);
            }
        }
        require(
            terminal(source.pidfd.as_ref().unwrap().as_raw_fd())?
                && source.reaped.is_some_and(|s| s.code().is_some())
                && source.eof == [true, true]
                && source.logs_synced,
            "outside source actual Child/EOF/log join incomplete",
        )?;
        let raw = unsafe { libc::kill(-(source.child.id() as i32), 0) };
        let error = (raw == -1).then(io::Error::last_os_error);
        require(
            raw == -1 && error.as_ref().and_then(io::Error::raw_os_error) == Some(libc::ESRCH),
            "outside source group remains",
        )?;
        check_no_children()?;
        self.check()?;
        ledger.append(json!({"kind":"outside-original-source-child-joined","wrapper_pid":source.child.id(),
            "wait_code":source.reaped.and_then(|s|s.code()),"eof":source.eof,"logs_synced":source.logs_synced,
            "stdout_bytes":source.stdout.len(),"stderr_bytes":source.stderr.len(),
            "origin":self.origin,"cutoff":self.cutoff}))?;
        self.complete = true;
        Ok(true)
    }
    pub fn held_descriptors(&self) -> Vec<RawFd> {
        let mut descriptors = vec![self.controller.as_raw_fd()];
        for query in self
            .terminal_query
            .iter()
            .map(|q| &q.query)
            .chain(self.command.iter())
            .chain(self.absence_query.iter().map(|q| &q.query))
        {
            descriptors.extend(query.pidfd.iter().map(AsRawFd::as_raw_fd));
            if let Some(child) = &query.child {
                descriptors.extend(child.stdout.iter().map(AsRawFd::as_raw_fd));
                descriptors.extend(child.stderr.iter().map(AsRawFd::as_raw_fd));
            }
        }
        descriptors
    }
    pub fn diagnostics(&self) -> Value {
        json!({"origin":self.origin,"cutoff":self.cutoff,"complete":self.complete,
        "controller_pidfd_held":true,"normal_stop_wait":self.normal_stop_wait,"failure":self.failure.as_ref().map(|f|&f.message),
        "terminal_query":self.terminal_query.as_ref().map(ManagerQuery::evidence),
        "command":self.command.as_ref().map(CommandQuery::evidence),"command_attempted":self.command_attempted,
        "command_done":self.command_done,"absence_query":self.absence_query.as_ref().map(ManagerQuery::evidence),
        "source_terminal_issued":false,"creator_constructed":false})
    }
}

// These forwards drive only existing actual query children. They neither
// clear the original command failure nor start/retry a manager operation.
impl ManagerStop {
    pub(in super::super) fn retire_local_custody(
        &mut self,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<QueryRetirement> {
        self.query.retire_custody(deadline, cause)
    }
}
impl ManagerForgetFailed {
    pub(in super::super) fn retire_local_custody(
        &mut self,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<QueryRetirement> {
        self.query.retire_custody(deadline, cause)
    }
}
