//! Retained S1 owners and a consuming terminal/leaf transition. No constructor
//! accepts a diagnostic terminal bit, failed parent owner, or serialized token.
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::time::Instant;

use super::Failure;
use super::Intent;
use super::adoption;
use super::guardian;
use super::owner;
use super::require;
use super::wire;
pub(super) mod successor;
pub(super) use successor::completed::CompletedSourceExport;

use super::entry::runtime_creation::RuntimeCreationCustody;

#[derive(Debug)]
enum SourceLauncher {
    Local(owner::Launcher),
    Remote(RuntimeCreationCustody),
}
impl SourceLauncher {
    fn pid(&self) -> i32 {
        match self {
            Self::Local(source) => source.child.id() as i32,
            Self::Remote(source) => source.source_pid(),
        }
    }
    fn drain(&mut self) -> io::Result<()> {
        match self {
            Self::Local(source) => source.drain(),
            Self::Remote(source) => source.poll_source_terminal().map(|_| ()),
        }
    }
    fn eof(&self) -> io::Result<[bool; 2]> {
        match self {
            Self::Local(source) => Ok(source.eof),
            Self::Remote(source) => {
                if source.source_terminal_received() {
                    source.verify_source_terminal()?;
                    Ok([true, true])
                } else {
                    Ok([false, false])
                }
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct SourceLogs {
    source: OwnedFd,
    keeper: OwnedFd,
}
impl SourceLogs {
    pub fn retain(source: OwnedFd, keeper: OwnedFd) -> Self {
        Self { source, keeper }
    }
}
#[derive(Debug)]
pub(super) struct InFlightSource {
    guardian: guardian::Holder,
    source: SourceLauncher,
    keeper: owner::Launcher,
    parent: wire::Channel,
    logs: SourceLogs,
    imported: adoption::KeeperExportReceiver,
    namespace: Option<owner::NamespaceCustody>,
    census: owner::CensusInventory,
    keeper_notice: Option<usize>,
    stop_complete: bool,
    parent_eof: bool,
    joined: bool,
    receive_census_open: bool,
    join_census_open: bool,
    deadline: Instant,
    native_deadline: u64,
    intent: Intent,
    unit: String,
    custody_writer: guardian::CustodyShutdown,
}
impl InFlightSource {
    /// Infallible move BEFORE the caller drives terminal/export operations.
    /// Bounds come from the actual original Holder, not a supplied new origin.
    #[expect(dead_code, reason = "Legacy local source constructor; the maintained entry uses remote runtime custody")]
    pub fn retain(
        guardian: guardian::Holder,
        source: owner::Launcher,
        keeper: owner::Launcher,
        parent: wire::Channel,
        logs: SourceLogs,
    ) -> Self {
        let (intent, unit, deadline, native_deadline, _) = guardian.original_context();
        let intent = intent.clone();
        let unit = unit.to_owned();
        Self {
            guardian,
            source: SourceLauncher::Local(source),
            keeper,
            parent,
            logs,
            imported: adoption::KeeperExportReceiver::retain(),
            namespace: None,
            census: owner::CensusInventory::retain(),
            keeper_notice: None,
            stop_complete: false,
            parent_eof: false,
            joined: false,
            receive_census_open: false,
            join_census_open: false,
            deadline,
            native_deadline,
            intent,
            unit,
            custody_writer: guardian::CustodyShutdown::default(),
        }
    }
    pub fn retain_remote(
        guardian: guardian::Holder,
        source: RuntimeCreationCustody,
        keeper: owner::Launcher,
        parent: wire::Channel,
        logs: SourceLogs,
        namespace: owner::NamespaceCustody,
    ) -> Self {
        let (intent, unit, deadline, native_deadline, _) = guardian.original_context();
        let intent = intent.clone();
        let unit = unit.to_owned();
        Self {
            guardian,
            source: SourceLauncher::Remote(source),
            keeper,
            parent,
            logs,
            imported: adoption::KeeperExportReceiver::retain(),
            namespace: Some(namespace),
            census: owner::CensusInventory::retain(),
            keeper_notice: None,
            stop_complete: false,
            parent_eof: false,
            joined: false,
            receive_census_open: false,
            join_census_open: false,
            deadline,
            native_deadline,
            intent,
            unit,
            custody_writer: guardian::CustodyShutdown::default(),
        }
    }
    fn check(&self) -> io::Result<()> {
        require(
            Instant::now() < self.deadline && guardian::monotonic_ns()? < self.native_deadline,
            "serial source original stage expired",
        )?;
        require(
            self.guardian.original_context().4 == guardian::Role::Guardian,
            "serial source requires original Guardian",
        )?;
        require(
            self.source.pid() != self.keeper.child.id() as i32,
            "serial source and Keeper alias",
        )
    }
    fn peer(&self) -> wire::Credentials {
        wire::Credentials {
            pid: self.keeper.child.id() as i32,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }
    fn required_descriptors(&self) -> Vec<RawFd> {
        let mut fds = self.imported.held_descriptors();
        if let Some(namespace) = &self.namespace {
            fds.extend(namespace.held_descriptors());
        }
        fds.extend([
            self.parent.fd.as_raw_fd(),
            self.logs.source.as_raw_fd(),
            self.logs.keeper.as_raw_fd(),
        ]);
        let mut launchers = vec![&self.keeper];
        match &self.source {
            SourceLauncher::Local(source) => launchers.push(source),
            SourceLauncher::Remote(source) => fds.extend(source.held_descriptors()),
        }
        for launcher in launchers {
            fds.extend(launcher.pidfd.iter().map(AsRawFd::as_raw_fd));
            fds.extend(launcher.log_files.iter().flatten().map(AsRawFd::as_raw_fd));
            fds.extend(launcher.child.stdout.iter().map(AsRawFd::as_raw_fd));
            fds.extend(launcher.child.stderr.iter().map(AsRawFd::as_raw_fd));
        }
        fds
    }
    fn census(&mut self) -> io::Result<()> {
        let required = self.required_descriptors();
        self.census.observe(&required, self.deadline)
    }
    fn progress(&mut self) -> io::Result<bool> {
        self.check()?;
        require(!self.joined, "serial source join cannot repeat")?;
        self.source.drain()?;
        self.keeper.drain()?;
        self.guardian.progress()?;
        if self.keeper_notice.is_none()
            && let Some(index) = self.parent.receive(4096)?
        {
                self.parent.packets[index].exact(0, self.peer())?;
                self.keeper_notice = Some(index);
            }
        if !self.guardian.source_exit_observed() || self.keeper_notice.is_none() {
            return Ok(false);
        }
        let expected = super::journal::canonical(&self.guardian.successful_exit_record()?)?;
        require(
            self.parent.packets[self.keeper_notice.unwrap()].bytes == expected,
            "Keeper original terminal invocation notice differs",
        )?;
        if !self.stop_complete {
            self.stop_complete = self.guardian.stop_source(self.deadline)?;
            if !self.stop_complete {
                return Ok(false);
            }
        }
        if !self.guardian.is_completed_stage() {
            return Ok(false);
        }
        if !self.imported.complete() {
            if !self.receive_census_open {
                self.census()?;
                self.receive_census_open = true;
            }
            let peer = self.peer();
            let original = self.guardian.check_completed_source()?;
            let received = self.imported.receive(&mut self.parent, peer, &original)?;
            if received {
                self.census()?;
                self.receive_census_open = false;
            }
            return Ok(false);
        }
        if !self.imported.acknowledged() {
            self.census()?;
            let original = self.guardian.check_completed_source()?;
            self.imported.acknowledge(&mut self.parent, &original)?;
            self.census()?;
            return Ok(false);
        }
        if !self.parent_eof {
            let Some(index) = self.parent.receive(2048)? else {
                return Ok(false);
            };
            let packet = &self.parent.packets[index];
            require(
                packet.raw.returned == 0
                    && packet.raw.errno.is_none()
                    && packet.bytes.is_empty()
                    && packet.credentials.is_empty()
                    && packet.rights.is_empty()
                    && packet.rights_messages == 0
                    && packet.flags == libc::MSG_CMSG_CLOEXEC,
                "terminal Keeper parent channel lacks strict EOF",
            )?;
            self.parent_eof = true;
        }
        if self.source.eof()? != [true, true] || self.keeper.eof != [true, true] {
            return Ok(false);
        }
        if !self.join_census_open {
            self.census()?;
            self.join_census_open = true;
        }
        // Actual isolated controller topology is mandatory. No unrelated live
        // guard/container child is exempted from either unchanged P_ALL/ECHILD.
        match &mut self.source {
            SourceLauncher::Local(source) => {
                if !guardian::reap_joined(
                    &mut [
                        (source, self.logs.source.as_raw_fd()),
                        (&mut self.keeper, self.logs.keeper.as_raw_fd()),
                    ],
                    self.deadline,
                )? {
                    return Ok(false);
                }
            }
            SourceLauncher::Remote(source) => {
                source.verify_source_terminal()?;
                // Source Child has always belonged to the outside Keeper. The
                // controller still joins EVERY actual local child, with the
                // same global ECHILD check and no population exemption.
                if !owner::terminal(
                    self.keeper
                        .pidfd
                        .as_ref()
                        .ok_or_else(|| io::Error::other("original Keeper pidfd absent"))?
                        .as_raw_fd(),
                )? {
                    return Ok(false);
                }
                self.keeper.reap_success(self.logs.keeper.as_raw_fd())?;
                source.verify_source_terminal()?;
            }
        }
        let original = self.guardian.check_completed_source()?;
        self.imported.verify(&original)?;
        self.census()?;
        self.check()?;
        self.joined = true;
        Ok(true)
    }
}
/// The only constructor is the pure phase move after InFlightSource::progress
/// completed every native join above. It grants cursor observation, not a leaf.
#[derive(Debug)]
pub(super) struct JoinedSource {
    original: InFlightSource,
}
impl JoinedSource {
    pub fn controls(&self) -> io::Result<&owner::Controls> {
        self.original.guardian.terminal_controls()
    }
    pub fn intent(&self) -> &Intent {
        &self.original.intent
    }
    pub fn deadline(&self) -> Instant {
        self.original.deadline
    }
    pub fn native_deadline(&self) -> u64 {
        self.original.native_deadline
    }
}
#[derive(Debug)]
struct Joining {
    source: JoinedSource,
    cursor: Option<adoption::CursorCustody>,
    observations: Option<owner::CreatedObservations>,
}
/// Private, non-Clone, non-serializable original successful ownership bundle.
#[derive(Debug)]
pub(super) struct SourceTerminal {
    source: JoinedSource,
    cursor: adoption::CursorCustody,
    // Retain the original terminal observations through the owner transition.
    _observations: owner::CreatedObservations,
}
impl SourceTerminal {
    pub(super) fn check_namespace_terminal(&self) -> io::Result<()> {
        self.source.original.check()?;
        require(
            self.source.original.joined && self.source.original.guardian.is_completed_stage(),
            "namespace transfer lacks actual original SourceTerminal",
        )
    }
    pub(super) fn take_namespace_owner(&mut self) -> io::Result<owner::NamespaceCustody> {
        self.check_namespace_terminal()?;
        let namespace = self
            .source
            .original
            .namespace
            .as_ref()
            .ok_or_else(|| io::Error::other("original terminal namespace already consumed"))?;
        namespace.completed(self.source.original.deadline)?;
        Ok(self.source.original.namespace.take().unwrap())
    }
}
#[derive(Debug)]
pub(super) struct SuccessorRequest {
    unit: String,
    arguments: Vec<std::ffi::OsString>,
}
impl SuccessorRequest {
    /// Configuration only. It cannot spawn or import a source capability.
    pub fn retain(unit: String, arguments: Vec<std::ffi::OsString>) -> Self {
        Self { unit, arguments }
    }
}
#[derive(Debug)]
struct LeafPlan {
    source: SourceTerminal,
    request: SuccessorRequest,
    intent_attempted: bool,
    intent_complete: bool,
    _keeper_spawn_attempted: bool,
    _source_spawn_attempted: bool,
    refused: Option<Failure>,
    // Captured from the actual completed Holder before its one-use handoff.
    // The new owner retains these bytes with both original journals; no
    // constructor accepts a peer's replacement for the completed history.
    created_pairs: Option<Vec<u8>>,
}
impl LeafPlan {
    fn prepare(&mut self) -> io::Result<()> {
        if let Some(error) = &self.refused {
            return Err(error.error());
        }
        let result = (|| {
            require(!self.intent_attempted, "leaf intent cannot repeat")?;
            self.intent_attempted = true;
            self.source.source.original.check()?;
            let nonce = self
                .request
                .unit
                .strip_prefix("hermit-accepted-")
                .and_then(|s| s.strip_suffix(".service"));
            require(
                nonce.is_some_and(super::valid_nonce)
                    && self.request.unit != self.source.source.original.unit,
                "successor unit is not a distinct exact accepted-purpose unit",
            )?;
            require(
                !self.request.arguments.is_empty() && self.request.arguments.len() <= 128,
                "successor argument population differs",
            )?;
            use std::os::unix::ffi::OsStrExt;
            require(
                self.request
                    .arguments
                    .iter()
                    .all(|arg| !arg.as_bytes().contains(&0)),
                "successor argument contains NUL",
            )?;
            // Both consumed original histories remain inside this installed
            // LeafPlan. Neither frozen Keeper file nor failed owner is reset.
            self.created_pairs = Some(
                self.source
                    .source
                    .original
                    .guardian
                    .check_completed_source()?
                    .pairs()
                    .to_vec(),
            );
            self.source.source.original.imported.consume_handoff()?;
            let request = serde_json::json!({"schema":"hermit-grouped-leaf-plan-v1",
                "unit":self.request.unit,"source_unit":self.source.source.original.unit,
                "nonce":self.source.source.original.intent.nonce,
                "incarnation":self.source.source.original.intent.incarnation,
                "stage_deadline":self.source.source.original.native_deadline,
                "arguments":self.request.arguments.iter().map(|arg| super::hex(arg.as_bytes())).collect::<Vec<_>>()});
            self.source
                .source
                .original
                .guardian
                .prepare_leaf_intent(request)?;
            self.source.source.original.check()?;
            self.intent_complete = true;
            Ok(())
        })();
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
}
#[derive(Debug)]
enum Phase {
    Source(InFlightSource),
    Joining(Joining),
    Terminal(SourceTerminal),
    Planned(LeafPlan),
}
#[derive(Debug)]
#[must_use = "serial source/leaf ownership must remain alive through every refusal"]
pub(super) struct SerialOwner {
    phase: Option<Phase>,
    refused: Option<Failure>,
    failure_origin: Option<u64>,
}
impl SerialOwner {
    pub fn retain(source: InFlightSource) -> Self {
        Self {
            phase: Some(Phase::Source(source)),
            refused: None,
            failure_origin: None,
        }
    }
    fn check(&self) -> io::Result<()> {
        if let Some(error) = &self.refused {
            return Err(error.error());
        }
        require(
            self.phase.is_some(),
            "serial custody transition was interrupted",
        )
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result
            && self.refused.is_none()
        {
                self.refused = Some(Failure::capture(error));
                self.failure_origin = guardian::monotonic_ns().ok();
            }
        result
    }
    pub fn take_leaf_namespace(&mut self) -> io::Result<owner::PreparedLeafNamespace> {
        let result = (|| {
            self.check()?;
            let Some(Phase::Terminal(source)) = &mut self.phase else {
                return Err(io::Error::other(
                    "leaf namespace requires unconsumed actual SourceTerminal",
                ));
            };
            owner::PreparedLeafNamespace::from_terminal(source)
        })();
        self.remember(result)
    }
    pub fn progress_source(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            if let Some(Phase::Source(source)) = &mut self.phase {
                if !source.progress()? {
                    return Ok(false);
                }
                // No fallible operation, callback, await or native effect occurs
                // between taking the installed owner and installing its join.
                let Some(Phase::Source(source)) = self.phase.take() else {
                    unreachable!()
                };
                self.phase = Some(Phase::Joining(Joining {
                    source: JoinedSource { original: source },
                    cursor: None,
                    observations: None,
                }));
            }
            let Some(Phase::Joining(joining)) = &mut self.phase else {
                return Err(io::Error::other(
                    "serial source transition repeated or after leaf consumption",
                ));
            };
            if joining.cursor.is_none() {
                joining.cursor = Some(adoption::CursorCustody::after_source_join(&joining.source)?);
            }
            let mut lease = joining
                .cursor
                .as_mut()
                .unwrap()
                .local_read(joining.source.controls()?)?;
            joining.observations =
                Some(owner::observe_created(&mut lease, joining.source.intent())?);
            joining.source.original.census()?;
            joining.source.original.check()?;
            let Some(Phase::Joining(joining)) = self.phase.take() else {
                unreachable!()
            };
            self.phase = Some(Phase::Terminal(SourceTerminal {
                source: joining.source,
                cursor: joining.cursor.unwrap(),
                _observations: joining.observations.unwrap(),
            }));
            Ok(true)
        })();
        self.remember(result)
    }
    pub fn local_custody_records(&self) -> io::Result<Vec<serde_json::Value>> {
        let original = match self
            .phase
            .as_ref()
            .ok_or_else(|| io::Error::other("serial original absent"))?
        {
            Phase::Source(source) => source,
            Phase::Joining(source) => &source.source.original,
            Phase::Terminal(source) => &source.source.original,
            Phase::Planned(source) => &source.source.source.original,
        };
        let mut records = original.guardian.local_custody_records("source-guardian");
        if let Some(namespace) = &original.namespace {
            records.push(serde_json::json!({
            "kind":"query","role":"source-namespace","record":namespace.evidence()}));
        }
        records.push(serde_json::json!({"kind":"launcher","role":"source-keeper","record":original.keeper.custody_evidence()}));
        Ok(records)
    }
    pub fn begin_s2_guardian(&mut self, guardian: &owner::Launcher) -> io::Result<()> {
        let Some(Phase::Planned(plan)) = &mut self.phase else {
            return Err(io::Error::other(
                "S2 Guardian registration lacks actual leaf plan",
            ));
        };
        match &mut plan.source.source.original.source {
            SourceLauncher::Remote(source) => source.begin_s2_guardian(guardian),
            SourceLauncher::Local(_) => Err(io::Error::other(
                "local qualifier has no runtime holder registration",
            )),
        }
    }
    pub fn receive_s2_guardian_ack(&mut self) -> io::Result<bool> {
        let Some(Phase::Planned(plan)) = &mut self.phase else {
            return Err(io::Error::other("S2 Guardian ACK lacks actual leaf plan"));
        };
        match &mut plan.source.source.original.source {
            SourceLauncher::Remote(source) => source.receive_s2_guardian_ack(),
            SourceLauncher::Local(_) => Err(io::Error::other(
                "local qualifier has no runtime holder registration",
            )),
        }
    }
    pub fn failure_notice_origin(&self) -> io::Result<u64> {
        let original = match self
            .phase
            .as_ref()
            .ok_or_else(|| io::Error::other("serial original absent"))?
        {
            Phase::Source(source) => source,
            Phase::Joining(source) => &source.source.original,
            Phase::Terminal(source) => &source.source.original,
            Phase::Planned(source) => &source.source.source.original,
        };
        match &original.source {
            SourceLauncher::Remote(source) => source.failure_notice_origin(),
            SourceLauncher::Local(_) => Err(io::Error::other(
                "local qualifier has no runtime failure notice",
            )),
        }
    }
    pub fn send_local_custody(
        &mut self,
        report: &serde_json::Value,
        file: std::os::fd::BorrowedFd<'_>,
    ) -> io::Result<()> {
        let original = match self
            .phase
            .as_mut()
            .ok_or_else(|| io::Error::other("serial original absent"))?
        {
            Phase::Source(source) => source,
            Phase::Joining(source) => &mut source.source.original,
            Phase::Terminal(source) => &mut source.source.original,
            Phase::Planned(source) => &mut source.source.source.original,
        };
        match &mut original.source {
            SourceLauncher::Remote(source) => source.send_local_custody(report, file),
            SourceLauncher::Local(_) => Err(io::Error::other(
                "local qualifier has no outside custody channel",
            )),
        }
    }
    pub fn retire_local_custody(
        &mut self,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<bool> {
        let deadline = if self.refused.is_some() {
            guardian::clip_custody_origin(deadline, self.failure_origin)?
        } else {
            deadline
        };
        let original = match self
            .phase
            .as_mut()
            .ok_or_else(|| io::Error::other("serial original source absent"))?
        {
            Phase::Source(source) => source,
            Phase::Joining(source) => &mut source.source.original,
            Phase::Terminal(source) => &mut source.source.original,
            Phase::Planned(source) => &mut source.source.source.original,
        };
        require(
            matches!(original.source, SourceLauncher::Remote(_)),
            "local qualifier has no outside source recovery owner",
        )?;
        let writer = original.custody_writer.progress(&original.parent);
        let queries = original.guardian.retire_local_custody(deadline, cause);
        // This query completed before the original first creation ACK, but its
        // actual owner remains here until the consuming leaf transfer.
        let namespace = original
            .namespace
            .as_mut()
            .map_or(Ok(owner::QueryRetirement::NoChild), |owner| {
                owner.retire_custody(deadline, cause)
            });
        // Drive actual query custody even when the retained shutdown refused.
        let complete = queries?;
        writer?;
        let namespace_done = !matches!(namespace?, owner::QueryRetirement::Pending);
        if !complete || !namespace_done {
            return Ok(false);
        }
        Ok(matches!(
            original
                .keeper
                .retire_custody(original.logs.keeper.as_raw_fd(), deadline, cause)?,
            owner::QueryRetirement::NoChild | owner::QueryRetirement::Retired
        ))
    }
    pub fn notify_runtime_failure(
        &mut self,
        origin: Option<u64>,
        cause: &io::Error,
    ) -> io::Result<()> {
        let original = match self
            .phase
            .as_mut()
            .ok_or_else(|| io::Error::other("serial original source absent"))?
        {
            Phase::Source(source) => source,
            Phase::Joining(source) => &mut source.source.original,
            Phase::Terminal(source) => &mut source.source.original,
            Phase::Planned(source) => &mut source.source.source.original,
        };
        match &mut original.source {
            SourceLauncher::Remote(source) => source.notify_failure(origin, cause),
            SourceLauncher::Local(_) => Err(io::Error::other(
                "local qualifier has no outside runtime source owner",
            )),
        }
    }
    /// Consume exactly once before validation/durable intent. A refused plan
    /// keeps the original source/archive/native resources in this owner.
    pub fn prepare_leaf_plan(&mut self, request: SuccessorRequest) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                matches!(self.phase, Some(Phase::Terminal(_))),
                "leaf plan lacks unconsumed SourceTerminal",
            )?;
            let Some(Phase::Terminal(source)) = self.phase.take() else {
                unreachable!()
            };
            self.phase = Some(Phase::Planned(LeafPlan {
                source,
                request,
                intent_attempted: false,
                intent_complete: false,
                _keeper_spawn_attempted: false,
                _source_spawn_attempted: false,
                refused: None,
                created_pairs: None,
            }));
            let Some(Phase::Planned(plan)) = &mut self.phase else {
                unreachable!()
            };
            plan.prepare()
        })();
        self.remember(result)
    }
}
