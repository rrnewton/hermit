//! Independent holder protocol shared by guardian and keeper. Each instance
//! owns its actual channel, creator capabilities, controls and durable journal;
//! none accepts a serialized terminal boolean as native authority.
use std::ffi::OsString;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;
use std::time::Duration;
use std::time::Instant;

use serde_json::json;

use super::Failure;
use super::Intent;
use super::hex;
use super::journal;
use super::owner;
use super::require;
use super::send_after_durability;
use super::wire;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Role {
    Guardian,
    Keeper,
}
impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Guardian => "guardian",
            Self::Keeper => "keeper",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Retained,
    Creator,
    Authenticating,
    Controls,
    Roles,
    CreationReady,
    Journal,
    Eof,
    Terminal,
}
// Fixed startup-only observations. No sample participates in admission or
// deadline calculation; a failed diagnostic clock remains explicitly unknown.
const ADMISSION_MARKS: [&str; 32] = [
    "creator_poll",
    "creator_packet",
    "creator_receipt_durable",
    "queries_start",
    "queries_started",
    "manager_observed",
    "image_observed",
    "authenticate_start",
    "authenticate_done",
    "namespace_progress",
    "namespace_done",
    "query_rows_start",
    "query_rows_done",
    "authenticated_row_start",
    "authenticated_row_done",
    "ack_row_start",
    "ack_row_done",
    "exec_policy_start",
    "exec_policy_done",
    "exec_send_start",
    "exec_send_done",
    "guardian_forward_row_start",
    "guardian_forward_row_done",
    "guardian_forward_sent",
    "controls_receive_result",
    "roles_started",
    "roles_observed",
    "controls_rows_start",
    "controls_rows_done",
    "ready_row_start",
    "ready_row_done",
    "ready_sent",
];
#[derive(Debug)]
struct AdmissionTiming {
    attempted: u32,
    failed_clock: u32,
    nanos: [Option<u64>; 32],
    errno: [Option<i32>; 32],
}
impl AdmissionTiming {
    fn new() -> Self {
        Self {
            attempted: 0,
            failed_clock: 0,
            nanos: [None; 32],
            errno: [None; 32],
        }
    }
    fn mark(&mut self, index: usize) {
        let bit = 1u32 << index;
        if self.attempted & bit != 0 {
            return;
        }
        self.attempted |= bit;
        let saved_errno = unsafe { *libc::__errno_location() };
        match monotonic_ns() {
            Ok(now) => self.nanos[index] = Some(now),
            Err(error) => {
                self.failed_clock |= bit;
                self.errno[index] = error.raw_os_error();
            }
        }
        unsafe {
            *libc::__errno_location() = saved_errno;
        }
    }
}

// One best-effort write to already owned stderr after retaining the failure.
// No new file, durability barrier, retry, or replacement error. Serialization
// or size refusal emits an omission record instead of truncated JSON.
pub(super) fn emit_startup_failure_diagnostic(value: &serde_json::Value) {
    let saved_errno = unsafe { *libc::__errno_location() };
    let mut bytes = [0u8; 4096];
    let mut cursor = io::Cursor::new(&mut bytes[..4095]);
    let serialized = serde_json::to_writer(&mut cursor, value);
    let count = cursor.position() as usize;
    let output = if serialized.is_ok() {
        bytes[count] = b'\n';
        &bytes[..count + 1]
    } else {
        b"{\"schema\":\"hermit-grouped-startup-timing-omitted-v1\",\"reason\":\"serialization-or-4096-byte-bound\"}\n".as_slice()
    };
    // Short/failed output cannot replace the original failure and is not
    // retried. Existing bounded launcher stderr retains whatever arrived.
    unsafe {
        libc::write(libc::STDERR_FILENO, output.as_ptr().cast(), output.len());
        *libc::__errno_location() = saved_errno;
    }
}

#[derive(Debug)]
struct FailedRetirement {
    native_origin: u64,
    deadline: Option<Instant>,
    complete: bool,
    source_admitted: Option<bool>,
}
#[derive(Debug)]
struct InflightRetirement {
    native_origin: u64,
    deadline: Instant,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    queries_retired: bool,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    query_failure: Option<Failure>,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    write_shutdown: Option<ShutdownAttempt>,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    shutdown_failure: Option<Failure>,
}
#[derive(Debug)]
enum ShutdownAttempt {
    IntentOnly,
    Submitted,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    Returned(wire::RawCall),
}
impl ShutdownAttempt {
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    fn observation(&self) -> serde_json::Value {
        match self {
            Self::IntentOnly => json!({"state":"intent-only"}),
            Self::Submitted => json!({"state":"submitted-result-unknown"}),
            Self::Returned(raw) => {
                json!({"state":"returned","returned":raw.returned,"errno":raw.errno})
            }
        }
    }
}
/// Constructed only after this actual Guardian has no Creator, has retired its
/// real endpoint/query custody, and durably joined an authenticated Keeper's
/// rejected-source history to the parent's independently captured native owner.
#[derive(Clone, Copy, Debug)]
#[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
enum EarlyCustodyKind {
    CapturedRejection,
    UncapturedCancellation,
}
#[derive(Debug)]
#[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
pub(super) struct EarlyCustodyAgreement {
    kind: EarlyCustodyKind,
    deadline: Instant,
    parent_identity: serde_json::Value,
    keeper_record: serde_json::Value,
    durable_record: serde_json::Value,
}
#[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
impl EarlyCustodyAgreement {
    pub(super) fn deadline(&self) -> Instant {
        self.deadline
    }
    pub(super) fn check_parent(
        &self,
        parent: &super::parent_launch::ParentFailedRetirement,
    ) -> io::Result<()> {
        require(
            Instant::now() < self.deadline,
            "early agreement original Guardian cutoff expired",
        )?;
        require(
            self.parent_identity == parent.identity_record()?,
            "early agreement replaced actual parent identity",
        )?;
        match self.kind {
            EarlyCustodyKind::CapturedRejection => parent.check_keeper_record(&self.keeper_record),
            EarlyCustodyKind::UncapturedCancellation => {
                parent.check_partial_keeper_record(&self.keeper_record)
            }
        }
    }
    pub(super) fn packet_schema(&self) -> &'static str {
        match self.kind {
            EarlyCustodyKind::CapturedRejection => "hermit-early-source-custody-agreement-v1",
            EarlyCustodyKind::UncapturedCancellation => {
                "hermit-uncaptured-source-custody-agreement-v1"
            }
        }
    }
    pub(super) fn ack_schema(&self) -> &'static str {
        match self.kind {
            EarlyCustodyKind::CapturedRejection => "hermit-early-source-custody-ack-v1",
            EarlyCustodyKind::UncapturedCancellation => "hermit-uncaptured-source-custody-ack-v1",
        }
    }
    pub(super) fn evidence(&self) -> &serde_json::Value {
        &self.durable_record
    }
}
#[derive(Debug)]
#[must_use = "retain outside operations until the joined launcher and holder cleanup"]
pub(super) struct Holder {
    intent: Intent,
    unit: String,
    deadline: Instant,
    native_deadline: u64,
    role: Role,
    channel: wire::Channel,
    journal: journal::Journal,
    entry: owner::EntryImage,
    creator: Option<owner::Creator>,
    namespace_required: bool,
    namespace_creator_cutoff: Option<u64>,
    namespace: Option<owner::NamespaceCustody>,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    partial_creator: Option<owner::PartialCreatorCustody>,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    partial_record: Option<serde_json::Value>,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    partial_custody_failure: Option<Failure>,
    controls: Option<owner::Controls>,
    manager: Option<owner::ManagerQuery>,
    image: Option<owner::EntryQuery>,
    roles: Option<owner::RoleQuery>,
    manager_snapshot: Option<owner::ManagerSnapshot>,
    image_snapshot: Option<owner::EntrySnapshot>,
    role_snapshot: Option<owner::RoleSnapshot>,
    // Both initial and terminal query owners stay retained. The finite state
    // machine invokes at most two manager queries, one image and one role query.
    terminal_manager: Option<owner::ManagerQuery>,
    terminal_snapshot: Option<owner::ManagerSnapshot>,
    terminal_natural: Option<bool>,
    stop: Option<owner::ManagerStop>,
    source_eof: bool,
    source_launcher: Option<owner::LauncherLease>,
    failed_retirement: Option<FailedRetirement>,
    forget_failed: Option<owner::ManagerForgetFailed>,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    failed_agreement: Option<serde_json::Value>,
    creation: Option<CreationGate>,
    unstarted_retirement: Option<FailedRetirement>,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    rejected_write_shutdown: Option<ShutdownAttempt>,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    rejected_shutdown_failure: Option<Failure>,
    guardian_forward_attempted: bool,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    uninitialized_passcred_failure: Option<wire::RawCall>,
    stage: Stage,
    refused: Option<Failure>,
    // Sample once at the first local failure. Unknown remains unknown; it must
    // never become a later cleanup origin merely because cleanup is requested.
    failure_origin: Option<u64>,
    inflight_retirement: Option<InflightRetirement>,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    early_agreement_attempted: bool,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    early_agreement_failure: Option<Failure>,
    local_custody: LocalCustodyRetirement,
    admission_timing: AdmissionTiming,
}
pub(super) fn clip_custody_origin(caller: Instant, origin: Option<u64>) -> io::Result<Instant> {
    let origin =
        origin.ok_or_else(|| io::Error::other("original custody failure origin unknown"))?;
    let sampled = Instant::now();
    let now = monotonic_ns()?;
    require(
        now >= origin && now - origin < 1_000_000_000,
        "original custody failure1s expired or future",
    )?;
    let cutoff = caller.min(sampled + Duration::from_nanos(1_000_000_000 - (now - origin)));
    require(Instant::now() < cutoff, "original custody cutoff expired")?;
    Ok(cutoff)
}
/// Custody-only one-use SHUT_WR. Neither a successful shutdown nor a failed
/// attempt grants source, manager-stop, journal-health or deletion authority.
#[derive(Debug, Default)]
pub(super) struct CustodyShutdown {
    attempt: Option<ShutdownAttempt>,
    refused: Option<Failure>,
}
impl CustodyShutdown {
    pub fn progress(&mut self, channel: &wire::Channel) -> io::Result<()> {
        if let Some(error) = &self.refused {
            return Err(error.error());
        }
        if self.attempt.is_some() {
            return Ok(());
        }
        let result = (|| {
            self.attempt = Some(ShutdownAttempt::IntentOnly);
            channel.validate()?;
            self.attempt = Some(ShutdownAttempt::Submitted);
            let raw = unsafe { libc::shutdown(channel.fd.as_raw_fd(), libc::SHUT_WR) };
            let error = (raw == -1).then(io::Error::last_os_error);
            self.attempt = Some(ShutdownAttempt::Returned(wire::RawCall {
                returned: raw as isize,
                errno: error.as_ref().and_then(io::Error::raw_os_error),
            }));
            if let Some(error) = error {
                return Err(error);
            }
            require(
                raw == 0,
                "custody writer shutdown returned unexpected result",
            )
        })();
        if let Err(error) = &result {
            self.refused = Some(Failure::capture(error));
        }
        result
    }
}
#[derive(Debug, Default)]
struct LocalCustodyRetirement {
    writer: CustodyShutdown,
    refused: Option<Failure>,
}
/// A fresh checked borrow; owning tokens are issued only by the serial join.
#[derive(Debug)]
pub(super) struct CompletedHolder<'a> {
    holder: &'a Holder,
    pairs: Vec<u8>,
}

/// Read-only archive of the original Guardian after its leaf handoff. This
/// cannot be supplied where CompletedHolder/SourceTerminal is required and
/// does not reset their consumed source admission.
#[derive(Debug)]
pub(super) struct LeafArchive<'a> {
    holder: &'a Holder,
    history: journal::SourceHistory,
    pairs: Vec<u8>,
}
impl LeafArchive<'_> {
    pub fn intent(&self) -> &Intent {
        &self.holder.intent
    }
    pub fn unit(&self) -> &str {
        &self.holder.unit
    }
    pub fn deadline(&self) -> Instant {
        self.holder.deadline
    }
    pub fn native_deadline(&self) -> u64 {
        self.holder.native_deadline
    }
    pub fn controls(&self) -> &owner::Controls {
        self.holder.controls.as_ref().unwrap()
    }
    pub fn creator_rights(&self) -> [BorrowedFd<'_>; 2] {
        let creator = self.holder.creator.as_ref().unwrap();
        [creator.pidfd.as_fd(), creator.directory.as_fd()]
    }
    pub fn store_rights(&self) -> io::Result<[BorrowedFd<'_>; 2]> {
        self.holder.journal.source_store_rights()
    }
    pub fn queries(&self) -> io::Result<[owner::CompletedQuery<'_>; 4]> {
        self.holder.completed_queries()
    }
    pub fn history(&self) -> &journal::SourceHistory {
        &self.history
    }
    pub fn pairs(&self) -> &[u8] {
        &self.pairs
    }
    pub fn completed_record(&self) -> io::Result<serde_json::Value> {
        self.holder.check()?;
        Ok(json!({"schema":"hermit-grouped-completed-holder-v1",
            "role":"guardian","nonce":self.holder.intent.nonce,
            "incarnation":self.holder.intent.incarnation,"unit":self.holder.unit,
            "stage_deadline":self.holder.native_deadline,
            "creator":self.holder.creator.as_ref().unwrap().evidence()?,
            "source_pidfd":owner::describe_fd(self.holder.creator.as_ref().unwrap().pidfd.as_raw_fd())?,
            "terminal_manager":self.holder.terminal_snapshot.as_ref().unwrap().evidence(),
            "pairs":serde_json::from_slice::<serde_json::Value>(&self.pairs)?,
            "next":35,"create_mask":0x1ffffu32}))
    }
}
impl CompletedHolder<'_> {
    pub fn role(&self) -> Role {
        self.holder.role
    }
    pub fn deadline(&self) -> Instant {
        self.holder.deadline
    }
    pub fn native_deadline(&self) -> u64 {
        self.holder.native_deadline
    }
    pub fn intent(&self) -> &Intent {
        &self.holder.intent
    }
    pub fn unit(&self) -> &str {
        &self.holder.unit
    }
    pub fn controls(&self) -> &owner::Controls {
        self.holder.controls.as_ref().unwrap()
    }
    pub fn creator_rights(&self) -> [BorrowedFd<'_>; 2] {
        let creator = self.holder.creator.as_ref().unwrap();
        [creator.pidfd.as_fd(), creator.directory.as_fd()]
    }
    pub fn queries(&self) -> io::Result<[owner::CompletedQuery<'_>; 4]> {
        self.holder.completed_queries()
    }
    pub fn pairs(&self) -> &[u8] {
        &self.pairs
    }
    pub fn record(&self) -> io::Result<serde_json::Value> {
        self.holder.check()?;
        Ok(json!({"schema":"hermit-grouped-completed-holder-v1",
            "role":self.holder.role.name(),"nonce":self.holder.intent.nonce,
            "incarnation":self.holder.intent.incarnation,"unit":self.holder.unit,
            "stage_deadline":self.holder.native_deadline,
            "creator":self.holder.creator.as_ref().unwrap().evidence()?,
            "source_pidfd":owner::describe_fd(self.holder.creator.as_ref().unwrap().pidfd.as_raw_fd())?,
            "terminal_manager":self.holder.terminal_snapshot.as_ref().unwrap().evidence(),
            "pairs":serde_json::from_slice::<serde_json::Value>(&self.pairs)?,
            "next":35,"create_mask":0x1ffffu32}))
    }
}
impl Holder {
    /// New source-entry configuration only. The retained Keeper owns the
    /// sealed artifact; this one send precedes any Creator or global effect.
    pub(super) fn forward_source_bridge(
        &mut self,
        configuration: &super::entry::BridgeConfiguration,
        file: BorrowedFd<'_>,
    ) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.role == Role::Keeper
                    && self.stage == Stage::Creator
                    && self.creator.is_none()
                    && self.channel.sends.is_empty(),
                "source bridge transfer is not the original pre-Creator send",
            )?;
            configuration.check(&self.intent, &self.unit, self.native_deadline)?;
            let bytes = journal::canonical(&serde_json::to_value(configuration)?)?;
            self.journal
                .store
                .append(json!({"kind":"sealed-source-bridge-transfer-intent",
                "configuration":configuration}))?;
            self.namespace_required = true;
            self.channel.send_once(&bytes, &[file])?;
            self.check()
        })();
        self.remember(result)
    }
    pub fn original_context(&self) -> (&Intent, &str, Instant, u64, Role) {
        (
            &self.intent,
            &self.unit,
            self.deadline,
            self.native_deadline,
            self.role,
        )
    }
    pub fn is_completed_stage(&self) -> bool {
        self.stage == Stage::Terminal && self.refused.is_none()
    }
    pub fn terminal_controls(&self) -> io::Result<&owner::Controls> {
        self.check()?;
        require(
            self.stage == Stage::Terminal && self.terminal_natural == Some(true),
            "original terminal controls are not complete",
        )?;
        let controls = self
            .controls
            .as_ref()
            .ok_or_else(|| io::Error::other("original terminal controls absent"))?;
        controls.check()?;
        Ok(controls)
    }
    pub fn successful_exit_record(&mut self) -> io::Result<serde_json::Value> {
        self.check()?;
        require(
            self.terminal_natural == Some(true) && self.terminal_snapshot.is_some(),
            "successful source exit has no original terminal snapshot",
        )?;
        self.journal.complete()?;
        self.terminal_manager
            .as_ref()
            .ok_or_else(|| io::Error::other("original terminal query absent"))?
            .completed_custody(self.deadline)?;
        let creator = self
            .creator
            .as_ref()
            .ok_or_else(|| io::Error::other("original source Creator absent"))?;
        creator.check_terminal_snapshot(self.terminal_snapshot.as_ref().unwrap(), true)?;
        Ok(json!({"schema":"hermit-grouped-source-exit-observed-v1",
            "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,
            "stage_deadline":self.native_deadline,"creator":creator.evidence()?,
            "terminal_manager":self.terminal_snapshot.as_ref().unwrap().evidence()}))
    }
    pub fn prepare_success_exit_notice(&mut self) -> io::Result<Vec<u8>> {
        let result = (|| {
            require(
                self.role == Role::Keeper,
                "source exit notice requires original Keeper",
            )?;
            let record = self.successful_exit_record()?;
            self.journal
                .store
                .append(json!({"kind":"source-exit-notice-intent","value":record}))?;
            self.check()?;
            journal::canonical(&record)
        })();
        self.remember(result)
    }
    /// This is a borrow of this actual Holder, never a constructor from a peer
    /// report. Existing complete/native/source-retirement gates stay in force.
    pub fn check_completed_source(&mut self) -> io::Result<CompletedHolder<'_>> {
        self.check()?;
        require(
            monotonic_ns()? < self.native_deadline,
            "completed Holder original native stage expired",
        )?;
        require(
            self.stage == Stage::Terminal
                && self.terminal_natural == Some(true)
                && self.source_eof
                && self.manager_snapshot.is_some()
                && self.image_snapshot.is_some()
                && self.role_snapshot.is_some()
                && self.terminal_snapshot.is_some()
                && self.failed_retirement.is_none()
                && self.inflight_retirement.is_none()
                && self.unstarted_retirement.is_none()
                && self.forget_failed.is_none(),
            "source Holder is not actually complete",
        )?;
        let creator = self
            .creator
            .as_ref()
            .ok_or_else(|| io::Error::other("completed source lacks Creator"))?;
        require(
            creator.admitted && self.source_retired()?,
            "completed source lacks admitted terminal unlinked custody",
        )?;
        creator.check_terminal_snapshot(self.terminal_snapshot.as_ref().unwrap(), true)?;
        self.controls
            .as_ref()
            .ok_or_else(|| io::Error::other("completed source lacks controls"))?
            .check()?;
        self.completed_queries()?;
        if self.namespace_required {
            self.namespace
                .as_ref()
                .ok_or_else(|| io::Error::other("source namespace custody absent"))?
                .completed(self.deadline)?;
        }
        match self.role {
            Role::Guardian => {
                self.stop
                    .as_ref()
                    .ok_or_else(|| io::Error::other("Guardian lacks original stop"))?
                    .completed_custody(self.deadline)?;
            }
            Role::Keeper => {
                require(self.stop.is_none(), "Keeper acquired Guardian stop custody")?;
                owner::check_no_children()?;
            }
        }
        let pairs = self.journal.canonical_created_pairs()?;
        self.check()?;
        require(
            monotonic_ns()? < self.native_deadline,
            "completed Holder original native stage expired",
        )?;
        Ok(CompletedHolder {
            holder: self,
            pairs,
        })
    }
    fn completed_queries(&self) -> io::Result<[owner::CompletedQuery<'_>; 4]> {
        Ok([
            self.manager
                .as_ref()
                .ok_or_else(|| io::Error::other("initial manager query absent"))?
                .completed_custody(self.deadline)?,
            self.image
                .as_ref()
                .ok_or_else(|| io::Error::other("initial image query absent"))?
                .completed_custody(self.deadline)?,
            self.roles
                .as_ref()
                .ok_or_else(|| io::Error::other("initial role query absent"))?
                .completed_custody(self.deadline)?,
            self.terminal_manager
                .as_ref()
                .ok_or_else(|| io::Error::other("terminal manager query absent"))?
                .completed_custody(self.deadline)?,
        ])
    }
    pub fn begin_success_export(&mut self) -> io::Result<()> {
        let result = (|| {
            require(
                self.role == Role::Keeper,
                "only original Keeper freezes terminal export",
            )?;
            let completed = self.check_completed_source()?;
            let record = completed.record()?;
            self.journal.begin_terminal_export(record)?;
            self.check()
        })();
        self.remember(result)
    }
    pub fn frozen_journal(&mut self) -> io::Result<journal::FrozenJournal<'_>> {
        self.check()?;
        require(
            self.role == Role::Keeper && self.stage == Stage::Terminal,
            "frozen archive requires original terminal Keeper",
        )?;
        self.journal.frozen_export()
    }
    pub fn prepare_leaf_intent(&mut self, request: serde_json::Value) -> io::Result<()> {
        let result = (|| {
            require(
                self.role == Role::Guardian,
                "leaf intent requires original Guardian",
            )?;
            self.check_completed_source()?;
            self.journal.begin_leaf_handoff(request)?;
            self.check()
        })();
        self.remember(result)
    }
    /// Infallibly move original capabilities into the caller's recovery scope.
    /// The outer owner separately retains the actual launcher Child/pidfd; a
    /// Holder never claims it can reap that launcher or close its pipes.
    pub fn retain(
        intent: Intent,
        unit: String,
        deadlines: (Instant, u64),
        role: Role,
        channels: (OwnedFd, OwnedFd),
        image: OwnedFd,
        arguments: Vec<OsString>,
    ) -> Self {
        let (deadline, native_deadline) = deadlines;
        let (channel, journal_directory) = channels;
        Self {
            journal: journal::Journal::retain(journal_directory, intent.clone()),
            intent,
            unit,
            deadline,
            native_deadline,
            role,
            channel: wire::Channel::retain(channel),
            entry: owner::EntryImage::retain(image, arguments),
            creator: None,
            namespace_required: false,
            namespace_creator_cutoff: None,
            namespace: None,
            partial_creator: None,
            partial_record: None,
            partial_custody_failure: None,
            controls: None,
            manager: None,
            image: None,
            roles: None,
            manager_snapshot: None,
            image_snapshot: None,
            role_snapshot: None,
            terminal_manager: None,
            terminal_snapshot: None,
            terminal_natural: None,
            stop: None,
            source_eof: false,
            source_launcher: None,
            failed_retirement: None,
            forget_failed: None,
            failed_agreement: None,
            creation: None,
            unstarted_retirement: None,
            rejected_write_shutdown: None,
            rejected_shutdown_failure: None,
            guardian_forward_attempted: false,
            uninitialized_passcred_failure: None,
            stage: Stage::Retained,
            refused: None,
            failure_origin: None,
            inflight_retirement: None,
            early_agreement_attempted: false,
            early_agreement_failure: None,
            local_custody: LocalCustodyRetirement::default(),
            admission_timing: AdmissionTiming::new(),
        }
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    fn actual_creator(&self) -> Option<&owner::Creator> {
        self.creator.as_ref().or_else(|| {
            self.partial_creator
                .as_ref()
                .map(owner::PartialCreatorCustody::original)
        })
    }
    fn check(&self) -> io::Result<()> {
        if let Some(error) = &self.refused {
            return Err(error.error());
        }
        require(
            Instant::now() < self.deadline,
            "holder original stage deadline expired",
        )
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(e) = &result {
            self.latch_failure(e);
        }
        result
    }
    fn latch_failure(&mut self, error: &io::Error) {
        if self.refused.is_none() {
            self.refused = Some(Failure::capture(error));
            self.failure_origin = monotonic_ns().ok();
            if self.admission_timing.attempted != 0 {
                emit_startup_failure_diagnostic(&json!({
                    "schema":"hermit-grouped-holder-startup-timing-at-failure-v1",
                    "nonce":self.intent.nonce,"role":self.role.name(),"stage":format!("{:?}",self.stage),
                    "stage_deadline":self.native_deadline,"creator_cutoff":self.namespace_creator_cutoff,
                    "first_failure_origin":self.failure_origin,"primary":error.to_string(),"errno":error.raw_os_error(),
                    "marks":ADMISSION_MARKS,"attempted":self.admission_timing.attempted,
                    "failed_clock":self.admission_timing.failed_clock,"nanos":self.admission_timing.nanos,
                    "clock_errno":self.admission_timing.errno,
                    "last_receive":self.channel.last_receive.map(|raw|json!({"returned":raw.returned,"errno":raw.errno})),
                    "last_packet":self.channel.packets.last().map(|packet|json!({
                        "returned":packet.raw.returned,"errno":packet.raw.errno,"bytes":packet.bytes.len(),
                        "flags":packet.flags,"retained_rights":packet.rights.len(),"rights_messages":packet.rights_messages,
                        "credentials":packet.credentials.iter().map(|c|(c.pid,c.uid,c.gid)).collect::<Vec<_>>()})),
                    "last_send":self.channel.sends.last().map(|sent|json!({"bytes":sent.bytes.len(),
                        "rights":sent.rights,"raw":sent.raw.map(|raw|json!({"returned":raw.returned,"errno":raw.errno}))}))}));
            }
        }
    }
    fn clip_failure_deadline(&self, caller: Instant) -> io::Result<Instant> {
        let caller = caller.min(self.deadline);
        if self.refused.is_none() {
            return Ok(caller);
        }
        let origin = self
            .failure_origin
            .ok_or_else(|| io::Error::other("holder original first-failure origin unknown"))?;
        let sampled = Instant::now();
        let now = monotonic_ns()?;
        require(
            now >= origin && now - origin < 1_000_000_000,
            "holder original first-failure1s expired or future",
        )?;
        let bound = caller.min(sampled + Duration::from_nanos(1_000_000_000 - (now - origin)));
        require(
            Instant::now() < bound,
            "holder original cleanup deadline expired",
        )?;
        Ok(bound)
    }
    pub fn local_custody_records(&self, prefix: &str) -> Vec<serde_json::Value> {
        let mut records = Vec::new();
        for (role, record) in [
            (
                "manager",
                self.manager.as_ref().map(owner::ManagerQuery::evidence),
            ),
            (
                "image",
                self.image.as_ref().map(owner::EntryQuery::evidence),
            ),
            (
                "namespace",
                self.namespace
                    .as_ref()
                    .map(owner::NamespaceCustody::evidence),
            ),
            ("roles", self.roles.as_ref().map(owner::RoleQuery::evidence)),
            (
                "terminal-manager",
                self.terminal_manager
                    .as_ref()
                    .map(owner::ManagerQuery::evidence),
            ),
            ("stop", self.stop.as_ref().map(owner::ManagerStop::evidence)),
            (
                "forget-failed",
                self.forget_failed
                    .as_ref()
                    .map(owner::ManagerForgetFailed::evidence),
            ),
        ] {
            if let Some(record) = record {
                records.push(
                    json!({"kind":"query","role":format!("{prefix}-{role}"),"record":record}),
                );
            }
        }
        records
    }
    /// Permanently refuse this live Holder, release only its writer, and join
    /// every already-owned query. The outside Keeper alone retires the source
    /// unit; this method never stops a unit, consumes source history or deletes.
    pub fn retire_local_custody(&mut self, caller: Instant, cause: &io::Error) -> io::Result<bool> {
        self.latch_failure(cause);
        let deadline = self.clip_failure_deadline(caller)?;
        let writer = self.local_custody.writer.progress(&self.channel);
        let mut outcomes = vec![writer.map(|()| owner::QueryRetirement::Retired)];
        for query in [&mut self.manager, &mut self.terminal_manager]
            .into_iter()
            .flatten()
        {
            outcomes.push(query.retire_custody(deadline, cause));
        }
        if let Some(query) = &mut self.image {
            outcomes.push(query.retire_custody(deadline, cause));
        }
        if let Some(query) = &mut self.namespace {
            outcomes.push(query.retire_custody(deadline, cause));
        }
        if let Some(query) = &mut self.roles {
            outcomes.push(query.retire_custody(deadline, cause));
        }
        if let Some(query) = &mut self.stop {
            outcomes.push(query.retire_local_custody(deadline, cause));
        }
        if let Some(query) = &mut self.forget_failed {
            outcomes.push(query.retire_local_custody(deadline, cause));
        }
        let mut complete = true;
        for outcome in outcomes {
            match outcome {
                Ok(owner::QueryRetirement::Pending) => complete = false,
                Ok(owner::QueryRetirement::Retired | owner::QueryRetirement::NoChild) => {}
                Err(error) => {
                    complete = false;
                    self.local_custody
                        .refused
                        .get_or_insert_with(|| Failure::capture(&error));
                }
            }
        }
        self.clip_failure_deadline(caller)?;
        if let Some(error) = &self.local_custody.refused {
            return Err(error.error());
        }
        Ok(complete)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    fn check_inflight_state(&self) -> io::Result<()> {
        require(
            matches!(
                self.stage,
                Stage::Creator | Stage::Authenticating | Stage::Controls | Stage::Roles
            ) && self.terminal_manager.is_none()
                && self.terminal_snapshot.is_none()
                && self.stop.is_none()
                && self.forget_failed.is_none()
                && self.failed_retirement.is_none()
                && self.unstarted_retirement.is_none()
                && self.journal.next == 1
                && self.journal.pairs.is_empty()
                && self.journal.pending.is_none()
                && self.journal.failed_pair.is_none()
                && self.journal.create_mask == 0,
            "inflight cleanup is outside original pre-journal custody",
        )
    }
    /// Cancellation is a permanent local refusal, not permission to resume
    /// protocol after query children finish. Existing refusal/origin wins.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn cancel_inflight(&mut self, cause: &io::Error) -> io::Result<()> {
        self.check_inflight_state()?;
        self.latch_failure(cause);
        Ok(())
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn begin_inflight_retirement(&mut self, caller: Instant) -> io::Result<()> {
        self.check_inflight_state()?;
        require(
            self.refused.is_some(),
            "inflight cleanup precedes original refusal",
        )?;
        let origin = self
            .failure_origin
            .ok_or_else(|| io::Error::other("inflight original failure origin unknown"))?;
        let sampled = Instant::now();
        let now = monotonic_ns()?;
        require(
            now >= origin && now - origin < 1_000_000_000,
            "inflight original first-failure1s expired or future",
        )?;
        let bound = (sampled + Duration::from_nanos(1_000_000_000 - (now - origin)))
            .min(self.deadline)
            .min(caller);
        if let Some(state) = &mut self.inflight_retirement {
            require(
                state.native_origin == origin,
                "inflight original failure origin changed",
            )?;
            state.deadline = state.deadline.min(bound);
        } else {
            self.inflight_retirement = Some(InflightRetirement {
                native_origin: origin,
                deadline: bound,
                queries_retired: false,
                query_failure: None,
                write_shutdown: None,
                shutdown_failure: None,
            });
        }
        self.inflight_deadline().map(|_| ())
    }
    fn inflight_deadline(&self) -> io::Result<Instant> {
        let state = self
            .inflight_retirement
            .as_ref()
            .ok_or_else(|| io::Error::other("inflight retirement was not begun"))?;
        require(
            self.refused.is_some() && self.failure_origin == Some(state.native_origin),
            "inflight cleanup lost original refusal or origin",
        )?;
        let now = monotonic_ns()?;
        require(
            now >= state.native_origin
                && now - state.native_origin < 1_000_000_000
                && Instant::now() < state.deadline,
            "inflight original retirement deadline expired",
        )?;
        Ok(state.deadline)
    }
    /// Each actual query is driven even if another query refuses cleanup. A
    /// completed typed snapshot reuses its original EOF/wait/group proof; failed
    /// or incomplete queries use custody-only retirement and are never parsed.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn progress_inflight_queries(&mut self) -> io::Result<bool> {
        self.check_inflight_state()?;
        let deadline = self.inflight_deadline()?;
        let cause = self.refused.as_ref().unwrap().error();
        let manager = if self.manager_snapshot.is_some() {
            Ok(owner::QueryRetirement::Retired)
        } else {
            self.manager
                .as_mut()
                .map_or(Ok(owner::QueryRetirement::NoChild), |q| {
                    q.retire_custody(deadline, &cause)
                })
        };
        let image = if self.image_snapshot.is_some() {
            Ok(owner::QueryRetirement::Retired)
        } else {
            self.image
                .as_mut()
                .map_or(Ok(owner::QueryRetirement::NoChild), |q| {
                    q.retire_custody(deadline, &cause)
                })
        };
        let roles = if self.role_snapshot.is_some() {
            Ok(owner::QueryRetirement::Retired)
        } else {
            self.roles
                .as_mut()
                .map_or(Ok(owner::QueryRetirement::NoChild), |q| {
                    q.retire_custody(deadline, &cause)
                })
        };
        let mut complete = true;
        let namespace = self
            .namespace
            .as_mut()
            .map_or(Ok(owner::QueryRetirement::NoChild), |q| {
                q.retire_custody(deadline, &cause)
            });
        for result in [manager, image, roles, namespace] {
            match result {
                Ok(owner::QueryRetirement::Pending) => complete = false,
                Ok(owner::QueryRetirement::NoChild | owner::QueryRetirement::Retired) => {}
                Err(error) => {
                    complete = false;
                    self.inflight_retirement
                        .as_mut()
                        .unwrap()
                        .query_failure
                        .get_or_insert_with(|| Failure::capture(&error));
                }
            }
        }
        self.inflight_deadline()?;
        let state = self.inflight_retirement.as_mut().unwrap();
        if let Some(error) = &state.query_failure {
            return Err(error.error());
        }
        state.queries_retired = complete;
        Ok(complete)
    }
    /// Close only this retained original endpoint's writer. This grants no
    /// Creator/admission/terminal authority and never discards incoming rights.
    /// Unknown syscall/durable-log outcome remains an occupied one-shot attempt.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn shutdown_inflight_writer(&mut self) -> io::Result<()> {
        let result = (|| {
            self.check_inflight_state()?;
            self.inflight_deadline()?;
            require(
                self.inflight_retirement
                    .as_ref()
                    .unwrap()
                    .write_shutdown
                    .is_none(),
                "inflight writer shutdown cannot be retried",
            )?;
            self.channel.validate()?;
            self.inflight_retirement.as_mut().unwrap().write_shutdown =
                Some(ShutdownAttempt::IntentOnly);
            self.journal.store.append(
                json!({"kind":"inflight-source-write-shutdown-intent","how":"SHUT_WR",
            "role":self.role.name(),"original_failure":self.refused.as_ref().unwrap().message,
            "native_origin":self.failure_origin,"creator_received":self.actual_creator().is_some(),
            "creator_captured":self.actual_creator().is_some_and(|c|c.captured),
            "creator_admitted":self.actual_creator().is_some_and(|c|c.admitted)}),
            )?;
            self.inflight_deadline()?;
            self.inflight_retirement.as_mut().unwrap().write_shutdown =
                Some(ShutdownAttempt::Submitted);
            let raw = unsafe { libc::shutdown(self.channel.fd.as_raw_fd(), libc::SHUT_WR) };
            let error = (raw < 0).then(io::Error::last_os_error);
            self.inflight_retirement.as_mut().unwrap().write_shutdown =
                Some(ShutdownAttempt::Returned(wire::RawCall {
                    returned: raw as isize,
                    errno: error.as_ref().and_then(io::Error::raw_os_error),
                }));
            self.journal
                .store
                .append(json!({"kind":"inflight-source-write-shutdown-result",
            "returned":raw,"errno":error.as_ref().and_then(io::Error::raw_os_error),
            "receive_custody_retained":true}))?;
            if let Some(error) = error {
                return Err(error);
            }
            require(raw == 0, "inflight writer shutdown result differs")?;
            self.inflight_deadline().map(|_| ())
        })();
        if let Err(error) = &result
            && let Some(state) = &mut self.inflight_retirement
        {
                state
                    .shutdown_failure
                    .get_or_insert_with(|| Failure::capture(error));
            }
        result
    }
    /// Diagnostic custody only. No complete-history or source capability exists
    /// merely because the query children have finished.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn inflight_observation(&self) -> io::Result<serde_json::Value> {
        self.check_inflight_state()?;
        self.inflight_deadline()?;
        let state = self.inflight_retirement.as_ref().unwrap();
        Ok(
            json!({"role":self.role.name(),"stage":format!("{:?}",self.stage),
            "original_failure":self.refused.as_ref().unwrap().message,"native_origin":state.native_origin,
            "queries_retired":state.queries_retired,"query_cleanup_failure":state.query_failure.as_ref().map(|e|&e.message),
            "manager":self.manager.as_ref().map(owner::ManagerQuery::evidence),
            "image":self.image.as_ref().map(owner::EntryQuery::evidence),
            "roles":self.roles.as_ref().map(owner::RoleQuery::evidence),
            "actual_snapshots_retained":[self.manager_snapshot.is_some(),self.image_snapshot.is_some(),self.role_snapshot.is_some()],
            "creator_received":self.actual_creator().is_some(),"creator_captured":self.actual_creator().is_some_and(|c|c.captured),
            "creator_admitted":self.actual_creator().is_some_and(|c|c.admitted),
            "controls_held":self.controls.as_ref().map(|c|c.fds.len()),
            "received_packet_count":self.channel.packets.len(),"received_rights_held":self.channel.packets.iter().map(|p|p.rights.len()).sum::<usize>(),
            "write_shutdown":state.write_shutdown.as_ref().map(ShutdownAttempt::observation),
            "shutdown_failure":state.shutdown_failure.as_ref().map(|e|&e.message),"history_complete_claimed":false,"source_terminal_issued":false,
            "provider_authority_created":false}),
        )
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn inflight_cleanup_deadline(&self) -> io::Result<Instant> {
        self.inflight_deadline()
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn keeper_unadmitted_observation(&self) -> io::Result<serde_json::Value> {
        require(
            self.role == Role::Keeper
                && self.stage == Stage::Authenticating
                && self.refused.is_some()
                && self.failure_origin.is_some()
                && self
                    .creator
                    .as_ref()
                    .is_some_and(|c| c.captured && !c.admitted)
                && !self.guardian_forward_attempted
                && self.channel.sends.is_empty()
                && self.controls.is_none(),
            "keeper first rejection lacks actual unadmitted nonforwarded custody",
        )?;
        Ok(
            json!({"guardian_forward_attempted":self.guardian_forward_attempted,"source_send_attempts":self.channel.sends.len(),
            "creator_admitted":self.creator.as_ref().unwrap().admitted,"native_origin":self.failure_origin,
            "original_failure":self.refused.as_ref().unwrap().message}),
        )
    }
    /// Authenticated parent coordination is retained as evidence; only this
    /// Holder's own original Creator/EOF/history predicates grant local custody.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn record_keeper_first_agreement(
        &mut self,
        value: &serde_json::Value,
    ) -> io::Result<Vec<u8>> {
        if let Some(error) = &self.early_agreement_failure {
            return Err(error.error());
        }
        let result = (|| {
            require(
                !self.early_agreement_attempted,
                "keeper early agreement cannot repeat",
            )?;
            self.early_agreement_attempted = true;
            self.keeper_unadmitted_observation()?;
            let own = self.failed_retirement_observation()?;
            let peer = &value["agreement"]["guardian"];
            let peer_failure = peer["original_failure"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| io::Error::other("early Guardian failure missing"))?;
            let peer_origin = peer["native_origin"]
                .as_u64()
                .ok_or_else(|| io::Error::other("early Guardian origin missing"))?;
            let sampled = Instant::now();
            let now = monotonic_ns()?;
            require(
                now >= peer_origin && now - peer_origin < 1_000_000_000,
                "early Guardian original1s expired or future",
            )?;
            let bound = sampled + Duration::from_nanos(1_000_000_000 - (now - peer_origin));
            let state = self.failed_retirement.as_mut().unwrap();
            state.deadline = state.deadline.map(|old| old.min(bound));
            if let Some(state) = &mut self.inflight_retirement {
                state.deadline = state.deadline.min(bound);
            }
            let parent = json!({"unit":own["creator"]["unit"],"nonce":own["creator"]["nonce"],"pid":own["creator"]["pid"],
                "invocation":own["creator"]["invocation"],"native_origin":own["native_origin"]});
            let expected = json!({"schema":"hermit-early-source-custody-agreement-v1","agreement":{
                "kind":"early-asymmetric-failed-custody-agreement","parent":parent,
                "guardian":{"creator_received":false,"source_eof":true,"history":own["history"],
                    "original_failure":peer_failure,"native_origin":peer_origin},
                "keeper":own,"manager_action":"reset-failed-original-captured-unit",
                "history_complete_claimed":false,"source_terminal_issued":false,"provider_authority_created":false}});
            require(
                *value == expected,
                "early agreement differs from exact own Keeper custody or peer grammar",
            )?;
            let bytes = journal::canonical(value)?;
            require(
                bytes.len() <= 4096,
                "early agreement exceeds original packet bound",
            )?;
            self.journal.store.verify()?;
            self.journal.store.append(
                json!({"kind":"independent-early-failed-custody-agreement","agreement":value}),
            )?;
            self.failed_retirement_deadline()?;
            use sha2::Digest;
            let ack = journal::canonical(
                &json!({"schema":"hermit-early-source-custody-ack-v1","role":"keeper","parent":parent,
                "agreement_sha256":hex(&sha2::Sha256::digest(&bytes))}),
            )?;
            require(
                ack.len() <= 4096,
                "early agreement ACK exceeds original packet bound",
            )?;
            Ok(ack)
        })();
        if let Err(error) = &result {
            self.early_agreement_failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn agree_keeper_first_rejection(
        &mut self,
        packet: &wire::Packet,
        keeper: wire::Credentials,
        parent: &super::parent_launch::ParentFailedRetirement,
    ) -> io::Result<EarlyCustodyAgreement> {
        if let Some(error) = &self.early_agreement_failure {
            return Err(error.error());
        }
        let result = (|| {
            require(
                !self.early_agreement_attempted,
                "early custody agreement cannot repeat",
            )?;
            self.early_agreement_attempted = true;
            self.check_inflight_state()?;
            self.inflight_deadline()?;
            parent.cutoff()?;
            require(
                self.role == Role::Guardian
                    && self.stage == Stage::Creator
                    && self.creator.is_none()
                    && self.manager.is_none()
                    && self.image.is_none()
                    && self.roles.is_none()
                    && self.controls.is_none()
                    && self.source_eof
                    && self.inflight_retirement.as_ref().unwrap().queries_retired,
                "early agreement lacks actual Guardian no-Creator custody",
            )?;
            packet.exact(0, keeper)?;
            let record: serde_json::Value = serde_json::from_slice(&packet.bytes)?;
            require(
                packet.bytes.len() <= 4096 && journal::canonical(&record)? == packet.bytes,
                "early Keeper record differs from original canonical packet bound",
            )?;
            parent.check_keeper_record(&record)?;
            let history = json!({"next":self.journal.next,"pending":self.journal.pending,"pairs":self.journal.pairs,
                "failed_pair":self.journal.failed_pair,"create_mask":self.journal.create_mask});
            require(
                history
                    == json!({"next":1,"pending":null,"pairs":[],"failed_pair":null,"create_mask":0})
                    && record["history"] == history,
                "early independent histories are not both actually empty",
            )?;
            self.journal.store.verify()?;
            let identity = parent.identity_record()?;
            let value = json!({"kind":"early-asymmetric-failed-custody-agreement","parent":identity,
                "guardian":{"creator_received":false,"source_eof":self.source_eof,"history":history,
                    "original_failure":self.refused.as_ref().unwrap().message,"native_origin":self.failure_origin},
                "keeper":record,"manager_action":"reset-failed-original-captured-unit",
                "history_complete_claimed":false,"source_terminal_issued":false,"provider_authority_created":false});
            self.journal.store.append(value.clone())?;
            self.inflight_deadline()?;
            parent.cutoff()?;
            Ok(EarlyCustodyAgreement {
                kind: EarlyCustodyKind::CapturedRejection,
                deadline: self.inflight_deadline()?,
                parent_identity: identity,
                keeper_record: record,
                durable_record: value,
            })
        })();
        if let Err(error) = &result {
            self.early_agreement_failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    /// Separate cleanup scope: neither initial query has produced a Snapshot,
    /// and the actual received Creator remains uncaptured and unadmitted.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    fn check_uncaptured_query_boundary(&self) -> io::Result<()> {
        self.check_inflight_state()?;
        let creator = self
            .actual_creator()
            .ok_or_else(|| io::Error::other("partial original Creator absent"))?;
        require(
            self.role == Role::Keeper
                && self.stage == Stage::Authenticating
                && !creator.captured
                && !creator.admitted
                && self.manager.is_some()
                && self.image.is_some()
                && self.manager_snapshot.is_none()
                && self.image_snapshot.is_none()
                && self.roles.is_none()
                && self.role_snapshot.is_none()
                && self.controls.is_none()
                && !self.guardian_forward_attempted
                && self.channel.sends.is_empty(),
            "partial cleanup lacks actual uncaptured unforwarded query custody",
        )
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn cancel_uncaptured_queries(&mut self, cause: &io::Error) -> io::Result<()> {
        // Existing primary refusal wins before any new cancellation is accepted.
        self.check()?;
        self.check_uncaptured_query_boundary()?;
        self.cancel_inflight(cause)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn keeper_uncaptured_observation(&self) -> io::Result<serde_json::Value> {
        self.check_uncaptured_query_boundary()?;
        require(
            self.refused.is_some() && self.failure_origin.is_some(),
            "partial cancellation original failure absent",
        )?;
        let creator = self.actual_creator().unwrap();
        Ok(
            json!({"guardian_forward_attempted":false,"source_send_attempts":0,"creator_received":true,
            "creator_captured":creator.captured,"creator_admitted":creator.admitted,
            "native_origin":self.failure_origin,"original_failure":self.refused.as_ref().unwrap().message}),
        )
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn begin_partial_creator_retirement(&mut self, caller: Instant) -> io::Result<()> {
        if let Some(error) = &self.partial_custody_failure {
            return Err(error.error());
        }
        let result = (|| {
            self.keeper_uncaptured_observation()?;
            require(
                self.partial_creator.is_none(),
                "partial original custody binding cannot repeat",
            )?;
            self.begin_inflight_retirement(caller)?;
            // Own the original descriptors in the cleanup-only type before its
            // first fallible native check. Never reconstruct or recapture them.
            self.partial_creator = Some(owner::PartialCreatorCustody::retain(
                self.creator.take().unwrap(),
            ));
            let deadline = self.inflight_deadline()?;
            self.partial_creator.as_mut().unwrap().begin(deadline)?;
            self.inflight_deadline().map(|_| ())
        })();
        if let Err(error) = &result {
            self.partial_custody_failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn progress_partial_creator_retirement(&mut self) -> io::Result<bool> {
        if let Some(error) = &self.partial_custody_failure {
            return Err(error.error());
        }
        let result = (|| {
            self.keeper_uncaptured_observation()?;
            let deadline = self.inflight_deadline()?;
            let state = self.inflight_retirement.as_ref().unwrap();
            require(
                state.queries_retired
                    && state.query_failure.is_none()
                    && state.shutdown_failure.is_none()
                    && matches!(
                        state.write_shutdown,
                        Some(ShutdownAttempt::Returned(wire::RawCall {
                            returned: 0,
                            errno: None
                        }))
                    ),
                "partial source cleanup precedes actual query retirement or writer revocation",
            )?;
            if !self
                .partial_creator
                .as_mut()
                .ok_or_else(|| io::Error::other("partial native owner absent"))?
                .terminal(deadline)?
            {
                return Ok(false);
            }
            if !self.observe_source_eof(deadline)? {
                return Ok(false);
            }
            self.journal.store.verify()?;
            if self.partial_record.is_none() {
                let queries = self.inflight_observation()?;
                require(
                    queries["actual_snapshots_retained"] == json!([false, false, false]),
                    "partial query cleanup adopted a successful Snapshot",
                )?;
                let creator = self
                    .partial_creator
                    .as_mut()
                    .unwrap()
                    .terminal_record(deadline)?;
                use sha2::Digest;
                let digest = hex(&sha2::Sha256::digest(journal::canonical(&queries)?));
                let record = json!({"schema":"hermit-uncaptured-source-custody-v1","native_origin":self.failure_origin,
                    "creator":creator,"creator_captured":false,"creator_admitted":false,"controls_held":null,
                    "source_eof":true,"source_retired":true,"queries_retired":true,"query_custody_sha256":digest,
                    "manager_snapshot_constructed":false,"history_complete_claimed":false,"source_terminal_issued":false,
                    "provider_authority_created":false,"original_failure":self.refused.as_ref().unwrap().message,
                    "history":{"next":self.journal.next,"pending":self.journal.pending,"pairs":self.journal.pairs,
                        "failed_pair":self.journal.failed_pair,"create_mask":self.journal.create_mask}});
                // Retain the exact report before the durable write. A failed
                // write occupies this attempt and cannot be retried as success.
                self.partial_record = Some(record.clone());
                self.journal.store.append(json!({"kind":"uncaptured-query-terminal-custody","queries":queries,"record":record}))?;
            }
            self.inflight_deadline()?;
            Ok(true)
        })();
        if let Err(error) = &result {
            self.partial_custody_failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn partial_creator_retirement_observation(&mut self) -> io::Result<serde_json::Value> {
        require(
            self.progress_partial_creator_retirement()?,
            "partial original source retirement remains pending",
        )?;
        Ok(self.partial_record.as_ref().unwrap().clone())
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn record_partial_keeper_agreement(
        &mut self,
        value: &serde_json::Value,
    ) -> io::Result<Vec<u8>> {
        if let Some(error) = &self.early_agreement_failure {
            return Err(error.error());
        }
        let result = (|| {
            require(
                !self.early_agreement_attempted,
                "partial keeper agreement cannot repeat",
            )?;
            self.early_agreement_attempted = true;
            let own = self.partial_creator_retirement_observation()?;
            let peer = &value["agreement"]["guardian"];
            let failure = peer["original_failure"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| io::Error::other("partial Guardian failure absent"))?;
            let origin = peer["native_origin"]
                .as_u64()
                .ok_or_else(|| io::Error::other("partial Guardian origin absent"))?;
            let sampled = Instant::now();
            let now = monotonic_ns()?;
            require(
                now >= origin && now - origin < 1_000_000_000,
                "partial Guardian original1s expired or future",
            )?;
            let bound = sampled + Duration::from_nanos(1_000_000_000 - (now - origin));
            let state = self.inflight_retirement.as_mut().unwrap();
            state.deadline = state.deadline.min(bound);
            self.inflight_deadline()?;
            let parent = json!({"unit":own["creator"]["unit"],"nonce":own["creator"]["nonce"],"pid":own["creator"]["pid"],
                "invocation":own["creator"]["invocation"],"native_origin":own["native_origin"]});
            let expected = json!({"schema":"hermit-uncaptured-source-custody-agreement-v1","agreement":{
                "kind":"uncaptured-asymmetric-failed-custody-agreement","parent":parent,
                "guardian":{"creator_received":false,"source_eof":true,"history":own["history"],
                    "original_failure":failure,"native_origin":origin},"keeper":own,
                "manager_action":"reset-failed-original-parent-captured-unit","history_complete_claimed":false,
                "source_terminal_issued":false,"provider_authority_created":false}});
            require(
                *value == expected,
                "partial agreement differs from exact Keeper custody or Guardian grammar",
            )?;
            let bytes = journal::canonical(value)?;
            require(
                bytes.len() <= 4096,
                "partial agreement exceeds original packet bound",
            )?;
            self.journal.store.verify()?;
            self.journal.store.append(
                json!({"kind":"independent-uncaptured-failed-custody-agreement","agreement":value}),
            )?;
            self.inflight_deadline()?;
            use sha2::Digest;
            let ack = journal::canonical(
                &json!({"schema":"hermit-uncaptured-source-custody-ack-v1","role":"keeper","parent":parent,
                "agreement_sha256":hex(&sha2::Sha256::digest(&bytes))}),
            )?;
            require(
                ack.len() <= 4096,
                "partial agreement ACK exceeds original packet bound",
            )?;
            Ok(ack)
        })();
        if let Err(error) = &result {
            self.early_agreement_failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn agree_partial_keeper_cancellation(
        &mut self,
        packet: &wire::Packet,
        keeper: wire::Credentials,
        parent: &super::parent_launch::ParentFailedRetirement,
    ) -> io::Result<EarlyCustodyAgreement> {
        if let Some(error) = &self.early_agreement_failure {
            return Err(error.error());
        }
        let result = (|| {
            require(
                !self.early_agreement_attempted,
                "partial Guardian agreement cannot repeat",
            )?;
            self.early_agreement_attempted = true;
            self.check_inflight_state()?;
            self.inflight_deadline()?;
            parent.cutoff()?;
            require(
                self.role == Role::Guardian
                    && self.stage == Stage::Creator
                    && self.actual_creator().is_none()
                    && self.manager.is_none()
                    && self.image.is_none()
                    && self.roles.is_none()
                    && self.controls.is_none()
                    && self.source_eof
                    && self.inflight_retirement.as_ref().unwrap().queries_retired,
                "partial agreement lacks actual Guardian no-Creator custody",
            )?;
            packet.exact(0, keeper)?;
            let record: serde_json::Value = serde_json::from_slice(&packet.bytes)?;
            require(
                packet.bytes.len() <= 4096 && journal::canonical(&record)? == packet.bytes,
                "partial Keeper report differs from canonical original packet bound",
            )?;
            parent.check_partial_keeper_record(&record)?;
            let history = json!({"next":self.journal.next,"pending":self.journal.pending,"pairs":self.journal.pairs,
                "failed_pair":self.journal.failed_pair,"create_mask":self.journal.create_mask});
            require(
                history
                    == json!({"next":1,"pending":null,"pairs":[],"failed_pair":null,"create_mask":0})
                    && record["history"] == history,
                "partial independent histories are not both actually empty",
            )?;
            self.journal.store.verify()?;
            let identity = parent.identity_record()?;
            let value = json!({"kind":"uncaptured-asymmetric-failed-custody-agreement","parent":identity,
                "guardian":{"creator_received":false,"source_eof":self.source_eof,"history":history,
                    "original_failure":self.refused.as_ref().unwrap().message,"native_origin":self.failure_origin},
                "keeper":record,"manager_action":"reset-failed-original-parent-captured-unit",
                "history_complete_claimed":false,"source_terminal_issued":false,"provider_authority_created":false});
            self.journal.store.append(value.clone())?;
            self.inflight_deadline()?;
            parent.cutoff()?;
            Ok(EarlyCustodyAgreement {
                kind: EarlyCustodyKind::UncapturedCancellation,
                deadline: self.inflight_deadline()?,
                parent_identity: identity,
                keeper_record: record,
                durable_record: value,
            })
        })();
        if let Err(error) = &result {
            self.early_agreement_failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    pub fn initialize(&mut self) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.stage == Stage::Retained,
                "holder cannot initialize twice",
            )?;
            require(
                self.deadline.saturating_duration_since(Instant::now()) <= Duration::from_secs(20),
                "holder stage allowance exceeds original20s",
            )?;
            let native_now = monotonic_ns()?;
            require(
                self.native_deadline > native_now
                    && self.native_deadline - native_now <= 20_000_000_000,
                "holder native stage allowance differs",
            )?;
            owner::protected_holder()?;
            self.entry.initialize()?;
            self.channel.validate()?;
            let unit_nonce = self
                .unit
                .strip_prefix("hermit-accepted-")
                .and_then(|s| s.strip_suffix(".service"));
            require(
                unit_nonce.is_some_and(super::valid_nonce),
                "holder source unit purpose differs",
            )?;
            self.journal.initialize(json!({"kind":"grouped-independent-holder","role":self.role.name(),"nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"unit":self.unit,"stage_deadline":self.native_deadline}))?;
            self.stage = Stage::Creator;
            Ok(())
        })();
        self.remember(result)
    }
    /// One nonblocking protocol step. The outer loop polls actual channel and
    /// launcher/query descriptors under the same original enclosing deadline.
    pub fn progress(&mut self) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            match self.stage {
                Stage::Creator => {
                    self.admission_timing.mark(0);
                    let Some(index) = self.channel.receive(512)? else {
                        return Ok(false);
                    };
                    self.admission_timing.mark(1);
                    self.creator = Some(owner::Creator::retain(
                        &mut self.channel.packets[index],
                        &self.intent,
                        &self.unit,
                    )?);
                    self.journal.store.append(json!({"kind":"creator-receipt","created":self.creator.as_ref().unwrap().receipt()}))?;
                    self.admission_timing.mark(2);
                    let creator = self.creator.as_ref().unwrap();
                    owner::pidfd_matches(creator.pidfd.as_raw_fd(), creator.peer.pid)?;
                    self.manager = Some(owner::ManagerQuery::retain(self.unit.clone()));
                    self.image = Some(owner::EntryQuery::retain(creator.peer.pid));
                    // Both child owners exist in retained state before either spawn.
                    self.stage = Stage::Authenticating;
                    self.admission_timing.mark(3);
                    self.manager.as_mut().unwrap().start()?;
                    self.image.as_mut().unwrap().start()?;
                    self.admission_timing.mark(4);
                    Ok(true)
                }
                Stage::Authenticating => {
                    let creator = self.creator.as_mut().unwrap();
                    owner::pidfd_matches(creator.pidfd.as_raw_fd(), creator.peer.pid)?;
                    if self.manager_snapshot.is_none() {
                        self.manager_snapshot =
                            self.manager.as_mut().unwrap().poll(self.deadline)?;
                        if self.manager_snapshot.is_some() {
                            self.admission_timing.mark(5);
                        }
                    }
                    if self.image_snapshot.is_none() {
                        self.image_snapshot = self.image.as_mut().unwrap().poll(self.deadline)?;
                        if self.image_snapshot.is_some() {
                            self.admission_timing.mark(6);
                        }
                    }
                    let (Some(manager), Some(image)) =
                        (&self.manager_snapshot, &self.image_snapshot)
                    else {
                        return Ok(false);
                    };
                    if !creator.admitted {
                        self.admission_timing.mark(7);
                        creator.authenticate(manager, &self.entry, image)?;
                        self.admission_timing.mark(8);
                    }
                    let mut execution_deadline = self.deadline;
                    if self.namespace_required {
                        if self.namespace.is_none() {
                            let Some(index) = self.channel.receive(1536)? else {
                                return Ok(false);
                            };
                            let packet = &mut self.channel.packets[index];
                            packet.exact(1, creator.peer)?;
                            let value: serde_json::Value = serde_json::from_slice(&packet.bytes)?;
                            let cutoff = value["creator_cutoff"].as_u64().ok_or_else(|| {
                                io::Error::other("source original Creator cutoff absent")
                            })?;
                            require(
                                packet.bytes
                                    == journal::canonical(&json!({
                                "schema":"hermit-grouped-source-namespace-v1", "nonce":self.intent.nonce,
                                "incarnation":self.intent.incarnation, "stage_deadline":self.native_deadline,
                                "unit":self.unit, "pid":creator.peer.pid, "creator_cutoff":cutoff}))?,
                                "source namespace offer differs",
                            )?;
                            self.namespace_creator_cutoff = Some(cutoff);
                            self.namespace = Some(owner::NamespaceCustody::retain(
                                packet.rights.remove(0),
                                creator.peer.pid,
                            ));
                        }
                        // This is the actual authenticated helper's already-sampled
                        // cutoff. It is never restarted at receipt or after queries.
                        let sampled = Instant::now();
                        let now = monotonic_ns()?;
                        let cutoff = self.namespace_creator_cutoff.unwrap();
                        require(
                            cutoff > now
                                && cutoff <= self.native_deadline
                                && cutoff - now <= 1_000_000_000,
                            "source original Creator1s expired or extended",
                        )?;
                        execution_deadline =
                            execution_deadline.min(sampled + Duration::from_nanos(cutoff - now));
                        self.admission_timing.mark(9);
                        if !self
                            .namespace
                            .as_mut()
                            .unwrap()
                            .progress(creator, execution_deadline)?
                        {
                            return Ok(false);
                        }
                        self.admission_timing.mark(10);
                        self.admission_timing.mark(11);
                        self.journal.store.append_creator_queries(
                            json!({"kind":"actual-source-namespace-query",
                                "original_creator_cutoff":self.namespace_creator_cutoff,
                                "query":self.namespace.as_ref().unwrap().completed_record(execution_deadline)?}),
                            json!({"kind":"actual-executable-query","observation":self.image.as_ref().unwrap().evidence()}))?;
                    } else {
                        self.admission_timing.mark(11);
                        self.journal.store.append(json!({"kind":"actual-executable-query","observation":self.image.as_ref().unwrap().evidence()}))?;
                    }
                    self.admission_timing.mark(12);
                    self.admission_timing.mark(13);
                    self.journal.store.append(
                        json!({"kind":"creator-authenticated","owner":creator.evidence()?}),
                    )?;
                    self.admission_timing.mark(14);
                    let packet = format!("EXEC {}\n", self.intent.nonce);
                    self.admission_timing.mark(15);
                    self.journal.store.append(
                        json!({"kind":"creator-ack-intent","packet":hex(packet.as_bytes())}),
                    )?;
                    self.admission_timing.mark(16);
                    self.admission_timing.mark(17);
                    creator.process_policy()?;
                    self.admission_timing.mark(18);
                    // Advance before the irreversible send attempt: no retry can
                    // reissue this authorization after an ambiguous return.
                    self.stage = Stage::Controls;
                    self.admission_timing.mark(19);
                    send_after_durability(
                        &mut self.channel,
                        execution_deadline,
                        packet.as_bytes(),
                    )?;
                    self.admission_timing.mark(20);
                    Ok(true)
                }
                Stage::Controls => {
                    let Some(index) = self.channel.receive(2048)? else {
                        return Ok(false);
                    };
                    self.admission_timing.mark(24);
                    self.controls = Some(owner::Controls::retain(
                        &mut self.channel.packets[index],
                        self.creator.as_ref().unwrap(),
                        &self.intent,
                    )?);
                    self.roles = Some(owner::RoleQuery::retain());
                    self.stage = Stage::Roles;
                    self.roles.as_mut().unwrap().start()?;
                    self.admission_timing.mark(25);
                    Ok(true)
                }
                Stage::Roles => {
                    let Some(named) = self.roles.as_mut().unwrap().poll(self.deadline)? else {
                        return Ok(false);
                    };
                    // Keep the actual successful query proof before any later
                    // identity/role/durability check can fail.
                    self.admission_timing.mark(26);
                    self.role_snapshot = Some(named);
                    let creator = self.creator.as_ref().unwrap();
                    owner::pidfd_matches(creator.pidfd.as_raw_fd(), creator.peer.pid)?;
                    self.controls
                        .as_mut()
                        .unwrap()
                        .authenticate(self.role_snapshot.as_ref().unwrap())?;
                    self.admission_timing.mark(27);
                    self.journal.store.append(json!({"kind":"independent-fixed-role-query","observation":self.roles.as_ref().unwrap().evidence()}))?;
                    self.journal.store.append(json!({"kind":"actual-controls-held","owner":creator.evidence()?,"controls":self.controls.as_ref().unwrap().evidence()?}))?;
                    self.admission_timing.mark(28);
                    if self.creation.is_some() {
                        self.stage = Stage::CreationReady;
                        return Ok(true);
                    }
                    let packet = journal::canonical(
                        &json!({"incarnation":self.intent.incarnation,"nonce":self.intent.nonce,"role":self.role.name(),"schema":"hermit-grouped-holder-ready-v1","stage_deadline":self.native_deadline}),
                    )?;
                    self.admission_timing.mark(29);
                    self.journal
                        .store
                        .append(json!({"kind":"holder-ready-intent","packet":hex(&packet)}))?;
                    self.admission_timing.mark(30);
                    creator.process_policy()?;
                    self.stage = Stage::Journal;
                    send_after_durability(&mut self.channel, self.deadline, &packet)?;
                    self.admission_timing.mark(31);
                    Ok(true)
                }
                Stage::CreationReady => {
                    if !self.creation.as_ref().unwrap().prepared {
                        return Ok(false);
                    }
                    self.controls.as_ref().unwrap().check()?;
                    let packet = journal::canonical(
                        &json!({"incarnation":self.intent.incarnation,"nonce":self.intent.nonce,
                        "role":self.role.name(),"schema":"hermit-grouped-holder-ready-v1","stage_deadline":self.native_deadline}),
                    )?;
                    self.admission_timing.mark(29);
                    self.journal
                        .store
                        .append(json!({"kind":"holder-ready-intent","packet":hex(&packet)}))?;
                    self.admission_timing.mark(30);
                    self.creator.as_ref().unwrap().process_policy()?;
                    self.stage = Stage::Journal;
                    send_after_durability(&mut self.channel, self.deadline, &packet)?;
                    self.admission_timing.mark(31);
                    Ok(true)
                }
                Stage::Journal => {
                    if self
                        .creation
                        .as_ref()
                        .is_some_and(|c| c.callback.is_some() || c.callback_attempted)
                    {
                        return Ok(false);
                    }
                    self.controls.as_ref().unwrap().check()?;
                    let creator = self.creator.as_ref().unwrap();
                    let Some(index) = self.channel.receive(1536)? else {
                        // systemd-run retains the keeper endpoint until the unit
                        // is stopped. Capture the actual terminal invocation
                        // before that stop; do not wait for EOF to query it.
                        return self.observe_source_exit(self.deadline, true);
                    };
                    let packet = &self.channel.packets[index];
                    self.journal.store.append(json!({"kind":"received-callback","flags":packet.flags,"packet":hex(&packet.bytes)}))?;
                    if packet.raw.returned == 0 {
                        require(
                            packet.bytes.is_empty()
                                && packet.credentials.is_empty()
                                && packet.rights.is_empty()
                                && packet.rights_messages == 0
                                && packet.flags == libc::MSG_CMSG_CLOEXEC,
                            "source EOF carried ancillary or truncation",
                        )?;
                        self.source_eof = true;
                        self.stage = Stage::Eof;
                        // Observe actual source result before checking whether
                        // its history completed. Stage::Eof retains the exact17
                        // pair gate; EOF cannot make a failed source successful.
                        return self.observe_source_exit(self.deadline, true);
                    }
                    packet.exact(0, creator.peer)?;
                    let state = creator.readback()?;
                    require(
                        !state.creator_terminal
                            && state.procs.as_ref().is_some_and(|p| {
                                p.lines().any(|s| s == creator.peer.pid.to_string())
                            }),
                        "journal creator left actual live cgroup",
                    )?;
                    let ack = self.journal.receive(&packet.bytes, Instant::now())?;
                    if let Some(gate) = &mut self.creation {
                        require(
                            gate.prepared,
                            "first creation ACK lacks installed cleanup owner",
                        )?;
                        if self.role == Role::Guardian || gate.external_keeper_mirror {
                            let frame: journal::Frame = serde_json::from_slice(&packet.bytes)?;
                            gate.callback_sequence = Some(frame.sequence);
                            gate.callback = Some(ack);
                            return Ok(true);
                        }
                    }
                    require(
                        !owner::terminal(creator.pidfd.as_raw_fd())?,
                        "journal creator terminal before ACK",
                    )?;
                    send_after_durability(&mut self.channel, self.deadline, &ack.bytes)?;
                    require(
                        !ack.native_failed,
                        "actual failed outcome retained; source remains unknown",
                    )?;
                    Ok(true)
                }
                Stage::Eof => {
                    if self.terminal_snapshot.is_none() {
                        return self.observe_source_exit(self.deadline, true);
                    }
                    if !self.source_retired()? {
                        return Ok(false);
                    }
                    self.journal.complete()?;
                    self.controls.as_ref().unwrap().check()?;
                    self.journal.store.verify()?;
                    self.stage = Stage::Terminal;
                    Ok(true)
                }
                Stage::Terminal => Ok(false),
                Stage::Retained => Err(io::Error::other("holder not initialized")),
            }
        })();
        self.remember(result)
    }
    /// Capture real creator exit while its manager InvocationID still exists.
    /// This separate custody operation preserves the original protocol refusal;
    /// it cannot turn failed creation into complete history or terminal authority.
    pub fn observe_source_exit(&mut self, deadline: Instant, natural: bool) -> io::Result<bool> {
        let deadline = self.clip_failure_deadline(deadline)?;
        let deadline = deadline.min(self.deadline);
        require(
            Instant::now() < deadline,
            "source exit observation exceeded original deadline",
        )?;
        let creator = self
            .creator
            .as_ref()
            .ok_or_else(|| io::Error::other("source creator custody absent"))?;
        require(creator.captured, "source creator was never captured")?;
        if let Some(previous) = self.terminal_natural {
            require(
                previous || !natural,
                "failed source exit cannot become natural success",
            )?;
        }
        if self.terminal_snapshot.is_some() {
            return Ok(false);
        }
        let result = (|| {
            let actual = creator.readback_progress()?;
            if !actual.creator_terminal() {
                return Ok(false);
            }
            if self.terminal_manager.is_none() {
                // Retain before spawn. Even while cgroup attributes are Pending,
                // capture this original terminal invocation without replacing it.
                self.terminal_manager = Some(owner::ManagerQuery::retain(self.unit.clone()));
                self.terminal_manager.as_mut().unwrap().start()?;
            }
            let Some(snapshot) = self.terminal_manager.as_mut().unwrap().poll(deadline)? else {
                return Ok(false);
            };
            // A completed query retains its exact raw output, EOFs and natural
            // wait. Re-poll never restarts its original two-second deadline.
            if creator.check_terminal_snapshot_progress(&snapshot, natural)?
                == owner::TerminalProgress::Pending
            {
                return Ok(false);
            }
            self.terminal_natural = Some(natural);
            self.journal.store.append(json!({"kind":"actual-source-exit-before-stop", "natural_required":natural, "manager":snapshot.evidence(), "creator":creator.receipt()}))?;
            require(
                Instant::now() < deadline,
                "source exit durability exceeded original deadline",
            )?;
            self.terminal_snapshot = Some(snapshot);
            Ok(true)
        })();
        self.remember(result)
    }
    pub fn source_exit_observed(&self) -> bool {
        self.terminal_snapshot.is_some()
    }
    /// Cancellation before a creator was ever received. The original source
    /// endpoint must reach actual EOF; absence of a creator record is not proof.
    /// Called only by Keeper while it still owns both original socketpair
    /// endpoints and has made no parent source-channel send attempt.
    /// Keeper can fail native setup after moving originals here but before
    /// initialize was called. Retain that caller's first setup cause without
    /// fabricating a journal attempt, creator admission or query ownership.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn retain_unforwarded_configuration_failure(
        &mut self,
        cause: &io::Error,
        passcred_failure: Option<wire::RawCall>,
    ) -> io::Result<()> {
        require(
            self.role == Role::Keeper
                && self.stage == Stage::Retained
                && self.creator.is_none()
                && self.controls.is_none()
                && self.manager.is_none()
                && self.image.is_none()
                && self.roles.is_none()
                && self.channel.sends.is_empty()
                && self.channel.packets.is_empty(),
            "unforwarded setup failure already has source admission",
        )?;
        if let Some(raw) = passcred_failure {
            require(
                raw.returned == -1
                    && raw.errno.is_some()
                    && raw.errno == cause.raw_os_error()
                    && self.refused.is_none()
                    && self.uninitialized_passcred_failure.is_none(),
                "uninitialized PASSCRED cause differs or was already retained",
            )?;
            self.uninitialized_passcred_failure = Some(raw);
        }
        self.latch_failure(cause);
        Ok(())
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn check_unforwarded_configuration(&self) -> io::Result<()> {
        require(
            self.role == Role::Keeper
                && self.stage == Stage::Retained
                && self.refused.is_some()
                && self.creator.is_none()
                && self.controls.is_none()
                && self.manager.is_none()
                && self.image.is_none()
                && self.roles.is_none()
                && self.channel.sends.is_empty()
                && self.channel.packets.is_empty(),
            "unforwarded configuration already has source or journal admission",
        )
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn observe_unforwarded_configuration_eof(&mut self, deadline: Instant) -> io::Result<bool> {
        let deadline = self.clip_failure_deadline(deadline)?;
        require(
            Instant::now() < deadline.min(self.deadline),
            "unforwarded EOF original deadline expired",
        )?;
        if self.source_eof {
            return Ok(true);
        }
        self.check_unforwarded_configuration()?;
        let index = if let Some(failure) = self.uninitialized_passcred_failure {
            if !self.channel.receive_uninitialized_eof(failure)? {
                return Ok(false);
            }
            self.channel.packets.len() - 1
        } else {
            let Some(index) = self.channel.receive(1536)? else {
                return Ok(false);
            };
            index
        };
        let packet = &self.channel.packets[index];
        // The journal may never have opened. Retain native packet custody in
        // memory; do not append through refusal or invent a durable journal.
        require(
            packet.raw.returned == 0
                && packet.raw.errno.is_none()
                && packet.bytes.is_empty()
                && packet.credentials.is_empty()
                && packet.rights.is_empty()
                && packet.rights_messages == 0
                && packet.flags == libc::MSG_CMSG_CLOEXEC,
            "unforwarded channel contains unconsumed bytes or rights",
        )?;
        require(
            Instant::now() < deadline.min(self.deadline),
            "unforwarded EOF completed after original deadline",
        )?;
        self.source_eof = true;
        Ok(true)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn unforwarded_configuration_observation(&self) -> io::Result<serde_json::Value> {
        require(
            self.source_eof
                && self.stage == Stage::Retained
                && self.refused.is_some()
                && self.creator.is_none()
                && self.controls.is_none()
                && self.manager.is_none()
                && self.image.is_none()
                && self.roles.is_none(),
            "unforwarded configuration custody incomplete",
        )?;
        let packet = self
            .channel
            .packets
            .last()
            .ok_or_else(|| io::Error::other("unforwarded EOF receipt missing"))?;
        Ok(
            json!({"source_eof":self.source_eof,"eof_raw":packet.raw.returned,"eof_errno":packet.raw.errno,
            "eof_flags":packet.flags,"eof_bytes":hex(&packet.bytes),"eof_rights":packet.rights.len(),
            "original_failure":self.refused.as_ref().unwrap().message,"original_errno":self.refused.as_ref().unwrap().errno,
            "uninitialized_passcred_failure":self.uninitialized_passcred_failure.map(|r|json!({"returned":r.returned,"errno":r.errno})),
            "uninitialized_eof_options":self.channel.uninitialized_eof_options,
            "normal_protocol_refused":self.channel.refused.is_some(),
            "journal_file_owned":self.journal.store.file.is_some(),"journal_directory_synced":self.journal.store.directory_synced,
            "journal_failure":self.journal.store.refused.as_ref().map(|f|&f.message),"journal_bytes_retained":self.journal.store.content.len(),
            "journal_syncs":self.journal.store.file_syncs,"creator_received":false,"query_children_started":false,"controls_held":false,
            "history_complete_claimed":false,"provider_authority_created":false}),
        )
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn begin_unstarted_retirement(&mut self, native_origin: u64) -> io::Result<()> {
        if let Some(state) = &self.unstarted_retirement {
            return require(
                state.native_origin == native_origin,
                "unstarted retirement cannot reset original origin",
            );
        }
        require(
            self.role == Role::Keeper
                && self.stage == Stage::Creator
                && self.refused.is_none()
                && self.creator.is_none()
                && self.manager.is_none()
                && self.image.is_none()
                && self.roles.is_none()
                && self.controls.is_none()
                && self.channel.packets.is_empty()
                && self.channel.sends.is_empty(),
            "unstarted retirement already acquired source ownership",
        )?;
        self.unstarted_retirement = Some(FailedRetirement {
            native_origin,
            deadline: None,
            complete: false,
            source_admitted: None,
        });
        self.latch_failure(&io::Error::other("source launch cancelled before creator"));
        let sampled = Instant::now();
        let now = monotonic_ns()?;
        require(
            native_origin <= now && now - native_origin < 1_000_000_000,
            "unstarted retirement original1s expired or future",
        )?;
        let deadline = self.clip_failure_deadline(
            sampled + Duration::from_nanos(1_000_000_000 - (now - native_origin)),
        )?;
        self.unstarted_retirement.as_mut().unwrap().deadline = Some(deadline);
        self.journal.store.append(json!({"kind":"unstarted-source-retirement-intent","native_origin":native_origin,"original_release_ns":1_000_000_000u64,"original_failure":self.refused.as_ref().unwrap().message}))?;
        self.unstarted_retirement_deadline().map(|_| ())
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    fn unstarted_retirement_deadline(&self) -> io::Result<Instant> {
        let state = self
            .unstarted_retirement
            .as_ref()
            .ok_or_else(|| io::Error::other("unstarted retirement was not begun"))?;
        let now = monotonic_ns()?;
        require(
            now >= state.native_origin && now - state.native_origin < 1_000_000_000,
            "unstarted retirement original1s expired or future",
        )?;
        let deadline = state
            .deadline
            .ok_or_else(|| io::Error::other("unstarted retirement origin was refused"))?;
        require(
            Instant::now() < deadline,
            "unstarted retirement original deadline expired",
        )?;
        self.clip_failure_deadline(deadline)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn progress_unstarted_retirement(&mut self) -> io::Result<bool> {
        let deadline = self.unstarted_retirement_deadline()?;
        require(
            self.creator.is_none()
                && self.manager.is_none()
                && self.image.is_none()
                && self.roles.is_none()
                && self.controls.is_none()
                && self
                    .refused
                    .as_ref()
                    .is_some_and(|e| e.message == "source launch cancelled before creator"),
            "unstarted retirement source ownership or failure changed",
        )?;
        if self.unstarted_retirement.as_ref().unwrap().complete {
            return Ok(true);
        }
        // The ordinary strict EOF path retains any unexpected packet and rights.
        // It refuses unconsumed contents instead of discarding them for cleanup.
        if !self.observe_source_eof(deadline)? {
            return Ok(false);
        }
        require(
            self.journal.next == 1
                && self.journal.pending.is_none()
                && self.journal.pairs.is_empty()
                && self.journal.failed_pair.is_none()
                && self.journal.create_mask == 0,
            "unstarted retirement history is not empty",
        )?;
        self.journal.store.verify()?;
        self.journal.store.append(json!({"kind":"unstarted-source-channel-retired","actual_eof":true,"creator_received":false,"query_children_started":false,"provider_authority_created":false}))?;
        self.unstarted_retirement_deadline()?;
        self.unstarted_retirement.as_mut().unwrap().complete = true;
        Ok(true)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn unstarted_retirement_observation(&mut self) -> io::Result<serde_json::Value> {
        self.unstarted_retirement_deadline()?;
        require(
            self.unstarted_retirement.as_ref().unwrap().complete,
            "unstarted source channel not retired",
        )?;
        self.journal.store.verify()?;
        Ok(
            json!({"schema":"hermit-unstarted-source-custody-v1","nonce":self.intent.nonce,
            "native_origin":self.unstarted_retirement.as_ref().unwrap().native_origin,"source_eof":self.source_eof,
            "creator_received":self.actual_creator().is_some(),"query_children_started":self.manager.is_some()||self.image.is_some()||self.roles.is_some(),
            "controls_held":self.controls.as_ref().map(|c|c.fds.len()),"original_failure":self.refused.as_ref().unwrap().message,
            "history":{"next":self.journal.next,"pending":self.journal.pending,"pairs":self.journal.pairs,"failed_pair":self.journal.failed_pair,"create_mask":self.journal.create_mask},
            "history_complete_claimed":false,"provider_authority_created":false}),
        )
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn record_unstarted_agreement(
        &mut self,
        keeper_record: &serde_json::Value,
    ) -> io::Result<()> {
        let own = self.unstarted_retirement_observation()?;
        require(
            keeper_record["observation"] == own,
            "unstarted agreement replaced holder observation",
        )?;
        self.journal
            .store
            .append(json!({"kind":"unstarted-source-parent-agreement","keeper":keeper_record}))?;
        self.unstarted_retirement_deadline().map(|_| ())
    }
    /// Custody-only cleanup after an actual sticky protocol/source refusal.
    /// This never completes a journal or creates source/provider authority.
    pub fn begin_failed_retirement(&mut self, native_origin: u64) -> io::Result<()> {
        self.begin_creator_retirement(native_origin, true)
    }
    /// A rejected creator retains kernel/manager/cgroup custody but never gains
    /// admission. The real completed initial queries remain owned; no ACK was
    /// attempted, so this path cannot authorize source creation or adoption.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn begin_rejected_creator_retirement(&mut self, native_origin: u64) -> io::Result<()> {
        self.begin_rejected_creator_role(native_origin, Role::Guardian)
    }
    /// Keeper-first image refusal has its own real captured Creator. It must
    /// not manufacture a Guardian Creator or forward that Guardian endpoint.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn keeper_rejected_cleanup_deadline(&self) -> io::Result<Instant> {
        self.keeper_unadmitted_observation()?;
        self.failed_retirement_deadline()
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn begin_keeper_rejected_creator_retirement(&mut self) -> io::Result<()> {
        require(
            !self.guardian_forward_attempted,
            "keeper rejection already attempted guardian forwarding",
        )?;
        let origin = self
            .failure_origin
            .ok_or_else(|| io::Error::other("keeper rejection original failure origin unknown"))?;
        self.begin_rejected_creator_role(origin, Role::Keeper)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    fn begin_rejected_creator_role(&mut self, native_origin: u64, role: Role) -> io::Result<()> {
        require(
            self.role == role
                && self.stage == Stage::Authenticating
                && self.manager_snapshot.is_some()
                && self.image_snapshot.is_some()
                && self.channel.sends.is_empty()
                && self.controls.is_none()
                && self.roles.is_none()
                && self
                    .creator
                    .as_ref()
                    .is_some_and(|c| c.captured && !c.admitted),
            "rejected creator retirement lacks original unadmitted custody",
        )?;
        self.begin_creator_retirement(native_origin, false)?;
        self.journal.store.append(json!({"kind":"rejected-creator-query-custody","creator_admitted":false,
            "guardian_ack_attempted":false,"manager":self.manager.as_ref().unwrap().evidence(),
            "image":self.image.as_ref().unwrap().evidence(),"original_failure":self.refused.as_ref().unwrap().message}))?;
        self.failed_retirement_deadline().map(|_| ())
    }
    /// Revoke only this retained guardian endpoint's writer after durable
    /// rejection. Keep its reader and every received right for strict real EOF.
    /// Native EOF makes the unchanged C expected-ACK path refuse immediately;
    /// no new wire frame or original deadline is introduced.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn shutdown_rejected_creator_writer(&mut self) -> io::Result<()> {
        self.shutdown_rejected_creator_role(Role::Guardian)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn shutdown_keeper_rejected_creator_writer(&mut self) -> io::Result<()> {
        require(
            !self.guardian_forward_attempted,
            "keeper rejection already attempted guardian forwarding",
        )?;
        self.shutdown_rejected_creator_role(Role::Keeper)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    fn shutdown_rejected_creator_role(&mut self, role: Role) -> io::Result<()> {
        let result = (|| {
            self.failed_retirement_deadline()?;
            require(
                self.role == role
                    && self.stage == Stage::Authenticating
                    && self.refused.is_some()
                    && self.failed_retirement.as_ref().unwrap().source_admitted == Some(false)
                    && self
                        .creator
                        .as_ref()
                        .is_some_and(|c| c.captured && !c.admitted)
                    && self.channel.sends.is_empty(),
                "rejected writer shutdown lacks unadmitted original custody",
            )?;
            require(
                self.rejected_write_shutdown.is_none(),
                "rejected writer shutdown cannot be retried",
            )?;
            require(
                !owner::terminal(self.creator.as_ref().unwrap().pidfd.as_raw_fd())?,
                "rejected creator already terminal before writer shutdown",
            )?;
            self.channel.validate()?;
            // Occupy the attempt before durable intent or syscall. Failure preserves
            // an unknown/failed attempt rather than permitting a second effect.
            self.rejected_write_shutdown = Some(ShutdownAttempt::IntentOnly);
            self.journal.store.append(json!({"kind":"rejected-source-write-shutdown-intent","how":"SHUT_WR",
            "original_failure":self.refused.as_ref().unwrap().message,"creator":self.creator.as_ref().unwrap().receipt(),
            "creator_admitted":false,"guardian_ack_attempted":false}))?;
            self.failed_retirement_deadline()?;
            self.rejected_write_shutdown = Some(ShutdownAttempt::Submitted);
            let raw = unsafe { libc::shutdown(self.channel.fd.as_raw_fd(), libc::SHUT_WR) };
            let error = (raw < 0).then(io::Error::last_os_error);
            self.rejected_write_shutdown = Some(ShutdownAttempt::Returned(wire::RawCall {
                returned: raw as isize,
                errno: error.as_ref().and_then(io::Error::raw_os_error),
            }));
            self.journal.store.append(json!({"kind":"rejected-source-write-shutdown-result","returned":raw,
            "errno":error.as_ref().and_then(io::Error::raw_os_error),"receive_custody_retained":true}))?;
            if let Some(error) = error {
                return Err(error);
            }
            require(raw == 0, "rejected writer shutdown result differs")?;
            self.failed_retirement_deadline().map(|_| ())
        })();
        if let Err(error) = &result {
            self.rejected_shutdown_failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn begin_creator_retirement(&mut self, native_origin: u64, admitted: bool) -> io::Result<()> {
        require(
            self.refused.is_some(),
            "failed retirement requires original refusal",
        )?;
        if let Some(previous) = &self.failed_retirement {
            require(
                previous.native_origin == native_origin,
                "failed retirement cannot reset original origin",
            )?;
            return require(
                previous.source_admitted == Some(admitted),
                "failed retirement cannot change admission class",
            );
        }
        self.failed_retirement = Some(FailedRetirement {
            native_origin,
            deadline: None,
            complete: false,
            source_admitted: Some(admitted),
        });
        let sampled = Instant::now();
        let now = monotonic_ns()?;
        require(
            native_origin <= now && now - native_origin < 1_000_000_000,
            "failed retirement original1s expired or future",
        )?;
        let local_origin = self.failure_origin.ok_or_else(|| {
            io::Error::other("failed retirement local first-failure origin unknown")
        })?;
        require(
            local_origin <= now && now - local_origin < 1_000_000_000,
            "failed retirement local first-failure1s expired or future",
        )?;
        let earliest_origin = native_origin.min(local_origin);
        let deadline = (sampled + Duration::from_nanos(1_000_000_000 - (now - earliest_origin)))
            .min(self.deadline);
        let deadline = self
            .inflight_retirement
            .as_ref()
            .map_or(deadline, |state| deadline.min(state.deadline));
        require(
            self.creator
                .as_ref()
                .is_some_and(|c| c.captured && c.admitted == admitted),
            "failed retirement lacks captured actual creator",
        )?;
        self.failed_retirement.as_mut().unwrap().deadline = Some(deadline);
        self.journal
            .store
            .append(json!({"kind":"failed-source-retirement-intent",
            "native_origin":native_origin,"original_release_ns":1_000_000_000u64,
            "original_failure":self.refused.as_ref().unwrap().message,
            "creator":self.creator.as_ref().unwrap().receipt(),"creator_admitted":admitted}))?;
        self.failed_retirement_deadline().map(|_| ())
    }
    fn failed_retirement_deadline(&self) -> io::Result<Instant> {
        let state = self
            .failed_retirement
            .as_ref()
            .ok_or_else(|| io::Error::other("failed retirement was not begun"))?;
        let now = monotonic_ns()?;
        require(
            now >= state.native_origin && now - state.native_origin < 1_000_000_000,
            "failed retirement original1s expired or future",
        )?;
        let local_origin = self.failure_origin.ok_or_else(|| {
            io::Error::other("failed retirement local first-failure origin unknown")
        })?;
        require(
            now >= local_origin && now - local_origin < 1_000_000_000,
            "failed retirement local first-failure1s expired or future",
        )?;
        let deadline = state
            .deadline
            .ok_or_else(|| io::Error::other("failed retirement origin was refused"))?;
        if self.inflight_retirement.is_some() {
            self.inflight_deadline()?;
        }
        require(
            Instant::now() < deadline,
            "failed retirement original deadline expired",
        )?;
        Ok(deadline)
    }
    pub fn progress_failed_retirement(&mut self) -> io::Result<bool> {
        let deadline = self.failed_retirement_deadline()?;
        require(
            self.refused.is_some(),
            "failed retirement lost original refusal",
        )?;
        require(
            self.creator.as_ref().is_some_and(|c| {
                c.captured
                    && Some(c.admitted) == self.failed_retirement.as_ref().unwrap().source_admitted
            }),
            "failed retirement original admission class changed",
        )?;
        if self.failed_retirement.as_ref().unwrap().complete {
            return Ok(true);
        }
        self.observe_source_exit(deadline, false)?;
        if !self.source_exit_observed() {
            return Ok(false);
        }
        if !self.observe_source_eof(deadline)? || !self.source_retired()? {
            return Ok(false);
        }
        if let Some(controls) = &self.controls {
            controls.check()?;
        }
        self.journal.store.verify()?;
        self.journal
            .store
            .append(json!({"kind":"failed-source-custody-retired",
            "original_failure":self.refused.as_ref().unwrap().message,
            "creator":self.creator.as_ref().unwrap().receipt(),
            "history_complete_claimed":false,"provider_authority_created":false}))?;
        self.failed_retirement_deadline()?;
        self.failed_retirement.as_mut().unwrap().complete = true;
        Ok(true)
    }
    /// This is a coordination record, never a capability constructor. Each peer
    /// must independently perform its native retirement and retain its history.
    pub fn failed_retirement_observation(&mut self) -> io::Result<serde_json::Value> {
        self.failed_retirement_deadline()?;
        require(
            self.failed_retirement.as_ref().unwrap().complete && self.refused.is_some(),
            "failed source custody remains incomplete",
        )?;
        self.journal.store.verify()?;
        Ok(json!({"schema":"hermit-failed-source-custody-v1",
            "native_origin":self.failed_retirement.as_ref().unwrap().native_origin,
            "creator":self.creator.as_ref().unwrap().captured_custody_identity()?,
            "creator_admitted":self.creator.as_ref().unwrap().admitted,
            "original_failure":self.refused.as_ref().unwrap().message,
            "actual_terminal_manager":self.terminal_snapshot.as_ref().unwrap().evidence(),
            "history":{"next":self.journal.next,"pending":self.journal.pending,
                "pairs":self.journal.pairs,"failed_pair":self.journal.failed_pair,
                "create_mask":self.journal.create_mask},
            "source_eof":self.source_eof,"source_retired":self.source_retired()?,
            "controls_held":self.controls.as_ref().map(|c|c.fds.len()),
            "history_complete_claimed":false,"provider_authority_created":false}))
    }
    /// Accept only an actual packet from the separately retained keeper peer.
    /// This coordinates retained custody records; it creates no provider right.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn accept_failed_keeper_observation(
        &mut self,
        packet: &wire::Packet,
        keeper: wire::Credentials,
    ) -> io::Result<serde_json::Value> {
        self.failed_retirement_deadline()?;
        require(
            self.role == Role::Guardian && self.failed_agreement.is_none(),
            "failed custody agreement requires fresh guardian",
        )?;
        packet.exact(0, keeper)?;
        let peer: serde_json::Value = serde_json::from_slice(&packet.bytes)?;
        require(
            journal::canonical(&peer)? == packet.bytes,
            "failed keeper observation not canonical",
        )?;
        let own = self.failed_retirement_observation()?;
        check_failed_observations(&own, &peer)?;
        let agreement = json!({"schema":"hermit-failed-source-history-agreement-v1", "guardian":own,"keeper":peer});
        require(
            journal::canonical(&agreement)?.len() <= 4096,
            "failed custody agreement exceeds original packet bound",
        )?;
        self.journal
            .store
            .append(json!({"kind":"independent-failed-custody-agreement","agreement":agreement}))?;
        self.failed_retirement_deadline()?;
        self.failed_agreement = Some(agreement.clone());
        Ok(agreement)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn forget_failed_source_unit(&mut self) -> io::Result<bool> {
        let deadline = self.failed_retirement_deadline()?;
        require(
            self.role == Role::Guardian
                && self.failed_retirement.as_ref().unwrap().complete
                && self.refused.is_some()
                && self.terminal_natural == Some(false)
                && self.failed_agreement.is_some(),
            "failed unit retirement lacks guardian agreed failed-source custody",
        )?;
        let creator = self.creator.as_ref().unwrap();
        let snapshot = self.terminal_snapshot.as_ref().unwrap();
        if self.forget_failed.is_none() {
            self.forget_failed = Some(owner::ManagerForgetFailed::retain(creator));
            self.journal
                .store
                .append(json!({"kind":"failed-unit-forget-intent",
                "original_failure":self.refused.as_ref().unwrap().message,
                "creator":creator.receipt(),"manager":snapshot.evidence()}))?;
            self.failed_retirement_deadline()?;
            self.forget_failed.as_mut().unwrap().start(
                creator,
                snapshot,
                self.source_launcher
                    .as_ref()
                    .ok_or_else(|| io::Error::other("source launcher custody absent"))?,
                deadline,
            )?;
        }
        self.forget_failed.as_mut().unwrap().poll(deadline)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn record_failed_agreement(&mut self, agreement: &serde_json::Value) -> io::Result<()> {
        self.failed_retirement_deadline()?;
        require(
            self.role == Role::Keeper && self.failed_agreement.is_none(),
            "failed keeper agreement reused",
        )?;
        let own = self.failed_retirement_observation()?;
        require(
            agreement["keeper"] == own,
            "failed keeper agreement replaced local history",
        )?;
        check_failed_observations(&agreement["guardian"], &own)?;
        self.journal
            .store
            .append(json!({"kind":"independent-failed-custody-agreement","agreement":agreement}))?;
        self.failed_retirement_deadline()?;
        self.failed_agreement = Some(agreement.clone());
        Ok(())
    }
    /// Bind the actual source launcher while it is still live. The parent keeps
    /// the original Child, pipes, pidfd and wait throughout joined retirement.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn bind_source_launcher(&mut self, launcher: &owner::Launcher) -> io::Result<()> {
        require(
            self.role == Role::Guardian && self.source_launcher.is_none(),
            "source launcher binding requires fresh guardian custody",
        )?;
        self.source_launcher = Some(launcher.source_lease()?);
        Ok(())
    }
    /// The outside Keeper retains the actual Child. This checked non-wait
    /// lease can only be imported from its continuously held live pidfd; it
    /// cannot construct a Child, grant wait custody or issue SourceTerminal.
    pub fn bind_remote_source_launcher(
        &mut self,
        launcher: owner::LauncherLease,
    ) -> io::Result<()> {
        require(
            self.role == Role::Guardian && self.source_launcher.is_none() && self.creator.is_none(),
            "remote source launcher binding requires fresh original Guardian",
        )?;
        self.source_launcher = Some(launcher);
        Ok(())
    }
    /// Only the guardian owns manager stop. The caller first obtains both
    /// holders' same-invocation observations; the keeper never races this effect.
    /// Source history/EOF and outer launcher waits remain separate obligations.
    pub fn stop_source(&mut self, deadline: Instant) -> io::Result<bool> {
        let deadline = self.clip_failure_deadline(deadline)?;
        let deadline = deadline.min(self.deadline);
        require(
            self.role == Role::Guardian && Instant::now() < deadline,
            "source stop lacks guardian or original deadline",
        )?;
        let creator = self
            .creator
            .as_ref()
            .ok_or_else(|| io::Error::other("source creator custody absent"))?;
        let snapshot = self
            .terminal_snapshot
            .as_ref()
            .ok_or_else(|| io::Error::other("source exit was not observed before stop"))?;
        let result = (|| {
            if self.stop.is_none() {
                self.stop = Some(owner::ManagerStop::retain(creator));
                self.journal.store.append(json!({"kind":"source-stop-intent", "manager":snapshot.evidence(), "creator":creator.receipt()}))?;
                require(
                    Instant::now() < deadline,
                    "source stop intent exceeded original deadline",
                )?;
            }
            let stop = self.stop.as_mut().unwrap();
            // A retained Waiting owner has no attempted command. Once Started,
            // only poll the same query; never try to spawn it a second time.
            if !stop.started()? {
                let progress = stop.try_start(
                    creator,
                    snapshot,
                    self.source_launcher
                        .as_ref()
                        .ok_or_else(|| io::Error::other("source launcher custody absent"))?,
                    deadline,
                )?;
                if progress == owner::StopStart::Pending {
                    return Ok(false);
                }
            }
            stop.poll(deadline)
        })();
        if let Err(error) = &result {
            // A failed intent must never be skipped by a later call merely
            // because its command owner was already retained.
            if let Some(stop) = &mut self.stop {
                stop.refuse(error);
            }
        }
        self.remember(result)
    }
    /// Drain the original source channel to its actual empty ancillary-free EOF
    /// after its immutable callbacks were consumed. Unexpected queued data is
    /// retained and refuses; this is never a callback-discarding cleanup loop.
    pub fn observe_source_eof(&mut self, deadline: Instant) -> io::Result<bool> {
        let deadline = self.clip_failure_deadline(deadline)?;
        require(
            Instant::now() < deadline.min(self.deadline),
            "source EOF original deadline expired",
        )?;
        if self.source_eof {
            return Ok(true);
        }
        let Some(i) = self.channel.receive(1536)? else {
            return Ok(false);
        };
        let packet = &self.channel.packets[i];
        self.journal.store.append(json!({"kind":"source-terminal-channel", "raw":packet.raw.returned, "packet":hex(&packet.bytes),"flags":packet.flags}))?;
        require(
            packet.raw.returned == 0
                && packet.bytes.is_empty()
                && packet.credentials.is_empty()
                && packet.rights.is_empty()
                && packet.rights_messages == 0
                && packet.flags == libc::MSG_CMSG_CLOEXEC,
            "source terminal channel contains unconsumed callbacks or ancillary data",
        )?;
        require(
            Instant::now() < deadline.min(self.deadline),
            "source EOF durability exceeded original deadline",
        )?;
        self.source_eof = true;
        Ok(true)
    }
    pub fn source_retired(&self) -> io::Result<bool> {
        let creator = self
            .creator
            .as_ref()
            .ok_or_else(|| io::Error::other("source creator custody absent"))?;
        match creator.readback_progress()? {
            owner::CgroupReadbackProgress::Pending(_) => Ok(false),
            owner::CgroupReadbackProgress::Observed(actual) => Ok(self.terminal_snapshot.is_some()
                && self.source_eof
                && actual.creator_terminal
                && actual.unlinked),
        }
    }
    /// Forward the parent's actual retained guardian endpoint once, only after
    /// this keeper authenticated the real source creator and attempted its ACK.
    pub fn forward_guardian(&mut self, endpoint: BorrowedFd<'_>) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.role == Role::Keeper
                    && self.stage == Stage::Controls
                    && !self.guardian_forward_attempted,
                "guardian forwarding is outside the actual creator stage",
            )?;
            self.guardian_forward_attempted = true;
            let packet = journal::canonical(&json!({"incarnation": self.intent.incarnation,
                "nonce": self.intent.nonce,"schema":"hermit-grouped-guardian-channel-v1",
                "stage_deadline":self.native_deadline}))?;
            self.admission_timing.mark(21);
            self.journal
                .store
                .append(json!({"kind":"guardian-channel-intent","packet":hex(&packet)}))?;
            self.admission_timing.mark(22);
            require(
                Instant::now() < self.deadline,
                "guardian forwarding durability exceeded original deadline",
            )?;
            self.channel.send_once(&packet, &[endpoint])?;
            self.admission_timing.mark(23);
            Ok(())
        })();
        self.remember(result)
    }
    pub fn creator_acknowledged(&self) -> bool {
        self.stage == Stage::Controls && self.refused.is_none()
    }
    pub fn controls_ready(&self) -> bool {
        self.stage == Stage::Journal && self.refused.is_none()
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub fn diagnostics(&self) -> io::Result<serde_json::Value> {
        Ok(
            json!({"role":self.role.name(),"stage":format!("{:?}",self.stage),"creator":self.actual_creator().map(owner::Creator::receipt),"partial_creator":self.partial_creator.as_ref().map(owner::PartialCreatorCustody::diagnostics),"partial_custody_failure":self.partial_custody_failure.as_ref().map(|e|&e.message),
                "partial_query_custody":self.partial_creator.as_ref().map(|_|json!({"manager":self.manager.as_ref().map(owner::ManagerQuery::evidence),
                    "image":self.image.as_ref().map(owner::EntryQuery::evidence),"roles":self.roles.as_ref().map(owner::RoleQuery::evidence),
                    "record":self.partial_record,"source_eof":self.source_eof,"inflight":self.inflight_retirement.as_ref().map(|r|json!({
                        "native_origin":r.native_origin,"queries_retired":r.queries_retired,"query_failure":r.query_failure.as_ref().map(|e|&e.message),
                        "write_shutdown":r.write_shutdown.as_ref().map(ShutdownAttempt::observation),"shutdown_failure":r.shutdown_failure.as_ref().map(|e|&e.message)}))})),"controls_held":self.controls.as_ref().map(|c|c.fds.len()),"journal_pairs":self.journal.pairs.len(),"failure":self.refused.as_ref().map(|e|&e.message)}),
        )
    }
}
pub(super) fn monotonic_ns() -> io::Result<u64> {
    let mut t = std::mem::MaybeUninit::<libc::timespec>::uninit();
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, t.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let t = unsafe { t.assume_init() };
    require(
        t.tv_sec >= 0 && (0..1_000_000_000).contains(&t.tv_nsec),
        "invalid monotonic timestamp",
    )?;
    (t.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(t.tv_nsec as u64))
        .ok_or_else(|| io::Error::other("monotonic overflow"))
}

/// Joint retirement of the two already owned launchers. Both actual waits are
/// retained before either unchanged serial finalizer requires global ECHILD.
/// Pipe EOF, pidfd terminality, CLD_EXITED/0, group absence and ECHILD remain
/// mandatory; no one actor's exit substitutes for the other or resets a clock.
pub(super) fn reap_joined(
    launchers: &mut [(&mut owner::Launcher, std::os::fd::RawFd); 2],
    deadline: Instant,
) -> io::Result<bool> {
    require(
        Instant::now() < deadline,
        "joined launcher original deadline expired",
    )?;
    require(
        launchers[0].0.child.id() != launchers[1].0.child.id(),
        "joined launchers alias one child",
    )?;
    let mut all = true;
    for (launcher, _) in launchers.iter_mut() {
        require(
            launcher.eof == [true, true],
            "joined launcher lacks both actual EOFs",
        )?;
        if launcher.reaped.is_none() {
            let fd = launcher
                .pidfd
                .as_ref()
                .ok_or_else(|| io::Error::other("joined launcher pidfd absent"))?;
            if !owner::terminal(fd.as_raw_fd())? {
                all = false;
                continue;
            }
            let pid = launcher.child.id() as libc::pid_t;
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as u32,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            require(
                unsafe { info.si_pid() } == pid
                    && info.si_code == libc::CLD_EXITED
                    && unsafe { info.si_status() } == 0,
                "joined launcher did not finish naturally",
            )?;
            launcher.reaped = launcher.child.try_wait()?;
            require(
                launcher.reaped.is_some_and(|s| s.success()),
                "joined actual wait failed",
            )?;
        }
    }
    if !all {
        return Ok(false);
    }
    for (launcher, directory) in launchers.iter_mut() {
        launcher.reap_success(*directory)?;
        require(
            Instant::now() < deadline,
            "joined final durability exceeded original deadline",
        )?;
    }
    Ok(true)
}

/// Join an actually failed source wrapper and naturally successful keeper after
/// both holders independently retired failed source custody. This cannot satisfy
/// reap_joined/reap_success or construct successful source/provider authority.
#[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
pub(super) fn reap_joined_failed_source(
    source: &mut owner::Launcher,
    source_directory: std::os::fd::RawFd,
    keeper: &mut owner::Launcher,
    keeper_directory: std::os::fd::RawFd,
    deadline: Instant,
) -> io::Result<bool> {
    require(
        Instant::now() < deadline,
        "failed joined original deadline expired",
    )?;
    require(
        source.child.id() != keeper.child.id(),
        "failed joined launchers alias",
    )?;
    for (launcher, expect_failure) in [(&mut *source, true), (&mut *keeper, false)] {
        require(
            launcher.eof == [true, true],
            "failed joined launcher lacks actual pipe EOFs",
        )?;
        if launcher.reaped.is_none() {
            let fd = launcher
                .pidfd
                .as_ref()
                .ok_or_else(|| io::Error::other("failed joined pidfd absent"))?;
            if !owner::terminal(fd.as_raw_fd())? {
                return Ok(false);
            }
            let pid = launcher.child.id() as i32;
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as u32,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            require(
                unsafe { info.si_pid() } == pid
                    && info.si_code == libc::CLD_EXITED
                    && (unsafe { info.si_status() } != 0) == expect_failure,
                "failed joined exact natural wait differs",
            )?;
            launcher.reaped = launcher.child.try_wait()?;
            require(
                launcher
                    .reaped
                    .is_some_and(|s| s.code().is_some_and(|c| (c != 0) == expect_failure)),
                "failed joined actual wait differs",
            )?;
        }
    }
    source.retire_failed(source_directory)?;
    keeper.reap_success(keeper_directory)?;
    require(
        Instant::now() < deadline,
        "failed joined durability exceeded original deadline",
    )?;
    Ok(true)
}

/// Independent histories and source identity must agree even when acquisition
/// stopped between atomic role groups. The original local failures and actual
/// control populations are retained separately, never replaced by an agreement.
#[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
pub(super) fn check_failed_observations(
    left: &serde_json::Value,
    right: &serde_json::Value,
) -> io::Result<()> {
    let a = left
        .as_object()
        .ok_or_else(|| io::Error::other("failed custody record is not an object"))?;
    let b = right
        .as_object()
        .ok_or_else(|| io::Error::other("failed custody peer record is not an object"))?;
    require(a.keys().eq(b.keys()), "failed custody field sets differ")?;
    for field in [
        "schema",
        "native_origin",
        "creator",
        "actual_terminal_manager",
        "history",
        "source_eof",
        "source_retired",
        "history_complete_claimed",
        "provider_authority_created",
    ] {
        require(
            a.get(field).is_some() && a.get(field) == b.get(field),
            "independent failed source histories or identities disagree",
        )?;
    }
    require(
        left["schema"] == "hermit-failed-source-custody-v1"
            && left["source_eof"] == true
            && left["source_retired"] == true
            && left["history_complete_claimed"] == false
            && left["provider_authority_created"] == false,
        "failed custody agreement claimed source/provider authority",
    )?;
    for record in [left, right] {
        require(
            record["creator_admitted"].is_boolean()
                && record["original_failure"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                && (record["controls_held"].is_null() || record["controls_held"] == 3),
            "failed custody local failure or atomic control population differs",
        )?;
    }
    Ok(())
}

#[derive(Debug)]
struct CreationGate {
    prepared: bool,
    callback: Option<journal::Acknowledgement>,
    callback_sequence: Option<u64>,
    callback_attempted: bool,
    external_keeper_mirror: bool,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    shutdown: Option<ShutdownAttempt>,
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    shutdown_failure: Option<Failure>,
}
impl Holder {
    /// Only the closed creation entry opts into this gate, before initialize or
    /// any source ownership is accepted. Legacy success/acquisition paths keep
    /// their exact state machine; the new entry cannot omit this transition.
    pub(super) fn install_creation_cleanup(&mut self) -> io::Result<()> {
        require(
            self.stage == Stage::Retained && self.creation.is_none(),
            "creation cleanup must be installed before source admission",
        )?;
        self.creation = Some(CreationGate {
            prepared: false,
            callback: None,
            callback_sequence: None,
            callback_attempted: false,
            external_keeper_mirror: false,
            shutdown: None,
            shutdown_failure: None,
        });
        Ok(())
    }
    /// The real successful source entry additionally withholds the Keeper C
    /// callback ACK until its exact dual durable prefix is mirrored outside
    /// the startup kill domain. Existing cleanup qualifier callers omit this.
    pub(super) fn install_external_creation_mirror(&mut self) -> io::Result<()> {
        require(
            self.role == Role::Keeper && self.stage == Stage::Retained,
            "external source mirror must precede Keeper admission",
        )?;
        let gate = self
            .creation
            .as_mut()
            .ok_or_else(|| io::Error::other("external source mirror lacks preparation"))?;
        require(
            !gate.external_keeper_mirror,
            "external source mirror repeated",
        )?;
        gate.external_keeper_mirror = true;
        Ok(())
    }
    pub(super) fn acknowledge_mirrored_keeper_callback(&mut self, sequence: u64) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.role == Role::Keeper
                    && self.stage == Stage::Journal
                    && self.held_creation_sequence() == Some(sequence),
                "mirrored Keeper callback identity differs",
            )?;
            self.creation_controls()?.check()?;
            let gate = self.creation.as_mut().unwrap();
            require(
                gate.external_keeper_mirror && gate.prepared && !gate.callback_attempted,
                "Keeper ACK lacks external mirror hold",
            )?;
            let ack = gate
                .callback
                .take()
                .ok_or_else(|| io::Error::other("Keeper callback not held"))?;
            gate.callback_attempted = true;
            require(
                !owner::terminal(self.creator.as_ref().unwrap().pidfd.as_raw_fd())?,
                "source terminal before mirrored Keeper ACK",
            )?;
            send_after_durability(&mut self.channel, self.deadline, &ack.bytes)?;
            require(
                !ack.native_failed,
                "actual failed outcome retained; source remains unknown",
            )?;
            let gate = self.creation.as_mut().unwrap();
            gate.callback_sequence = None;
            gate.callback_attempted = false;
            Ok(())
        })();
        self.remember(result)
    }
    pub(super) fn awaiting_creation_preparation(&self) -> bool {
        self.stage == Stage::CreationReady && self.creation.as_ref().is_some_and(|c| !c.prepared)
    }
    pub(super) fn creation_controls(&self) -> io::Result<&owner::Controls> {
        require(
            self.creation.is_some()
                && matches!(
                    self.stage,
                    Stage::CreationReady | Stage::Journal | Stage::Eof
                ),
            "creation controls outside retained cleanup entry",
        )?;
        let controls = self
            .controls
            .as_ref()
            .ok_or_else(|| io::Error::other("creation controls absent"))?;
        controls.check()?;
        Ok(controls)
    }
    pub(super) fn install_creation_preparation(
        &mut self,
        proof: super::cleanup::Preparation,
    ) -> io::Result<()> {
        require(
            self.awaiting_creation_preparation(),
            "creation preparation reused or late",
        )?;
        proof.check(
            self.creation_controls()?,
            &self.intent,
            self.role,
            self.native_deadline,
        )?;
        self.creation.as_mut().unwrap().prepared = true;
        Ok(())
    }
    pub(super) fn creation_source_rights(&self) -> io::Result<[BorrowedFd<'_>; 2]> {
        require(
            self.creation.is_some(),
            "source Store export outside cleanup entry",
        )?;
        self.journal.source_store_rights()
    }
    pub(super) fn creation_creator(&self) -> io::Result<&owner::Creator> {
        self.check()?;
        require(
            self.creation.is_some(),
            "creation Creator outside original cleanup owner",
        )?;
        let creator = self
            .creator
            .as_ref()
            .ok_or_else(|| io::Error::other("creation Creator absent"))?;
        require(creator.admitted, "creation Creator was not authenticated")?;
        Ok(creator)
    }
    pub(super) fn source_namespace(&self) -> io::Result<&owner::NamespaceCustody> {
        self.check()?;
        require(
            self.namespace_required && self.role == Role::Keeper,
            "namespace export requires original source Keeper",
        )?;
        let namespace = self
            .namespace
            .as_ref()
            .ok_or_else(|| io::Error::other("source namespace absent"))?;
        namespace.completed(self.deadline)?;
        Ok(namespace)
    }
    pub(super) fn creation_source_history(&mut self) -> io::Result<journal::SourceHistory> {
        require(
            self.creation.is_some(),
            "source history outside cleanup entry",
        )?;
        self.journal.source_history()
    }
    pub(super) fn held_creation_sequence(&self) -> Option<u64> {
        self.creation.as_ref().and_then(|c| c.callback_sequence)
    }
    pub(super) fn acknowledge_creation_when_ready(
        &mut self,
        ready: Option<&super::cleanup::Readiness>,
    ) -> io::Result<bool> {
        require(
            self.stage == Stage::Journal
                && self
                    .creation
                    .as_ref()
                    .is_some_and(|gate| gate.callback.is_some()),
            "creation readiness wait lacks original held callback",
        )?;
        let Some(ready) = ready else {
            return Ok(false);
        };
        self.acknowledge_creation_callback(ready)?;
        Ok(true)
    }
    pub(super) fn acknowledge_creation_callback(
        &mut self,
        ready: &super::cleanup::Readiness,
    ) -> io::Result<()> {
        let result = (|| {
            self.check()?;
            require(
                self.role == Role::Guardian && self.stage == Stage::Journal,
                "creation ACK outside original Guardian",
            )?;
            ready.check(
                self.creation_controls()?,
                &self.intent,
                self.native_deadline,
            )?;
            let gate = self.creation.as_mut().unwrap();
            require(
                gate.prepared && !gate.callback_attempted,
                "creation ACK lacks preparation or was attempted",
            )?;
            let ack = gate
                .callback
                .take()
                .ok_or_else(|| io::Error::other("no retained creation callback"))?;
            gate.callback_attempted = true;
            require(
                !owner::terminal(self.creator.as_ref().unwrap().pidfd.as_raw_fd())?,
                "journal creator terminal before ACK",
            )?;
            send_after_durability(&mut self.channel, self.deadline, &ack.bytes)?;
            require(
                !ack.native_failed,
                "actual failed outcome retained; source remains unknown",
            )?;
            let gate = self.creation.as_mut().unwrap();
            gate.callback_sequence = None;
            gate.callback_attempted = false;
            Ok(())
        })();
        self.remember(result)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub(super) fn cancel_creation_after_prefix(&mut self, cause: &io::Error) -> io::Result<u64> {
        require(
            !self.journal.pairs.is_empty(),
            "creation cancellation lacks nonempty acknowledged prefix",
        )?;
        self.cancel_held_creation(cause)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub(super) fn cancel_creation_before_first_ack(
        &mut self,
        cause: &io::Error,
    ) -> io::Result<u64> {
        require(
            self.journal.pairs.is_empty()
                && self.journal.failed_pair.is_none()
                && self.held_creation_sequence() == Some(1)
                && self
                    .journal
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.write.role == 1 && p.write.started == 0),
            "zero-write cancellation is after first creation ACK",
        )?;
        self.cancel_held_creation(cause)
    }
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    fn cancel_held_creation(&mut self, cause: &io::Error) -> io::Result<u64> {
        require(
            self.role == Role::Guardian && self.stage == Stage::Journal,
            "creation prefix cancellation outside original Guardian",
        )?;
        let gate = self
            .creation
            .as_ref()
            .ok_or_else(|| io::Error::other("creation cleanup not installed"))?;
        require(
            gate.prepared
                && !gate.callback_attempted
                && gate.callback.is_some()
                && self.journal.pending.is_some(),
            "creation cancellation lacks nonempty acknowledged prefix and held next intent",
        )?;
        self.latch_failure(cause);
        self.journal.refuse_creation(cause);
        let origin = self
            .failure_origin
            .ok_or_else(|| io::Error::other("creation first failure origin unknown"))?;
        self.begin_failed_retirement(origin)?;
        let result = (|| {
            let deadline = self.failed_retirement_deadline()?;
            require(
                self.creation.as_ref().unwrap().shutdown.is_none(),
                "creation SHUT_WR cannot repeat",
            )?;
            self.creation.as_mut().unwrap().shutdown = Some(ShutdownAttempt::IntentOnly);
            self.journal.store.append(
                json!({"kind":"failed-prefix-shutdown-intent","origin":origin,
                "sequence":self.creation.as_ref().unwrap().callback_sequence,
                "original_failure":self.refused.as_ref().unwrap().message}),
            )?;
            require(
                Instant::now() < deadline,
                "creation cutoff expired before SHUT_WR",
            )?;
            self.creation.as_mut().unwrap().shutdown = Some(ShutdownAttempt::Submitted);
            let raw = unsafe { libc::shutdown(self.channel.fd.as_raw_fd(), libc::SHUT_WR) };
            let error = (raw != 0).then(io::Error::last_os_error);
            self.creation.as_mut().unwrap().shutdown =
                Some(ShutdownAttempt::Returned(wire::RawCall {
                    returned: raw as isize,
                    errno: error.as_ref().and_then(io::Error::raw_os_error),
                }));
            if let Some(error) = error {
                return Err(error);
            }
            self.journal
                .store
                .append(json!({"kind":"failed-prefix-shutdown-return",
                "call":self.creation.as_ref().unwrap().shutdown.as_ref().unwrap().observation()}))?;
            self.failed_retirement_deadline()?;
            Ok(origin)
        })();
        if let Err(error) = &result {
            self.creation
                .as_mut()
                .unwrap()
                .shutdown_failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    pub(super) fn cancel_creation_peer(
        &mut self,
        cause: &io::Error,
        origin: u64,
    ) -> io::Result<()> {
        require(
            self.role == Role::Keeper
                && self.creation.as_ref().is_some_and(|c| c.prepared)
                && matches!(self.stage, Stage::Journal | Stage::Eof),
            "cleanup cancellation lacks original prepared Keeper",
        )?;
        self.latch_failure(cause);
        self.journal.refuse_creation(cause);
        self.begin_failed_retirement(origin)
    }
    pub(super) fn creation_original_cutoff(&self) -> io::Result<u64> {
        self.failed_retirement_deadline()?;
        let supplied = self.failed_retirement.as_ref().unwrap().native_origin;
        let local = self
            .failure_origin
            .ok_or_else(|| io::Error::other("creation local first failure origin unknown"))?;
        supplied
            .min(local)
            .checked_add(1_000_000_000)
            .map(|n| n.min(self.native_deadline))
            .ok_or_else(|| io::Error::other("creation original cutoff overflow"))
    }
    pub(super) fn creation_terminal_record(&mut self) -> io::Result<serde_json::Value> {
        require(
            self.creation.is_some(),
            "creation terminal observation outside cleanup entry",
        )?;
        let mut record = self.failed_retirement_observation()?;
        require(
            record["creator_admitted"] == true && record["controls_held"] == 3,
            "creation terminal observation lacks original admitted controls",
        )?;
        record.as_object_mut().unwrap().remove("history");
        record["schema"] = json!("hermit-creation-prefix-terminal-v1");
        record["source_store"] = self.journal.source_history()?.commitment();
        record["cleanup_cutoff"] = json!(self.creation_original_cutoff()?);
        Ok(record)
    }
    /// Separate private asymmetric authority; the old equal-history field and
    /// comparator are not consulted, filled, cleared, or changed here.
    #[expect(dead_code, reason = "Retained typed failed-creator recovery is not yet integrated with the active startup route")]
    pub(super) fn forget_failed_creation(
        &mut self,
        agreement: &super::cleanup::AsymmetricAgreement,
    ) -> io::Result<bool> {
        let deadline = self.failed_retirement_deadline()?;
        require(
            self.role == Role::Guardian
                && self.failed_retirement.as_ref().unwrap().complete
                && self.refused.is_some()
                && self.terminal_natural == Some(false),
            "creation unit reset lacks actual failed-source custody",
        )?;
        agreement.check_holder(&self.creation_terminal_record()?, deadline)?;
        let creator = self.creator.as_ref().unwrap();
        let snapshot = self.terminal_snapshot.as_ref().unwrap();
        if self.forget_failed.is_none() {
            self.forget_failed = Some(owner::ManagerForgetFailed::retain(creator));
            self.forget_failed.as_mut().unwrap().start(
                creator,
                snapshot,
                self.source_launcher
                    .as_ref()
                    .ok_or_else(|| io::Error::other("source launcher custody absent"))?,
                deadline,
            )?;
        }
        self.forget_failed.as_mut().unwrap().poll(deadline)
    }
}

impl Holder {
    /// Read the original journal only after the distinct successful leaf
    /// transition. All ordinary completion/export entrypoints keep their old
    /// refusal; this creates no source/creator/stop/open authority.
    pub(super) fn completed_leaf_archive(&mut self) -> io::Result<LeafArchive<'_>> {
        self.check()?;
        require(
            self.role == Role::Guardian
                && self.stage == Stage::Terminal
                && self.terminal_natural == Some(true)
                && self.source_eof
                && self.manager_snapshot.is_some()
                && self.image_snapshot.is_some()
                && self.role_snapshot.is_some()
                && self.terminal_snapshot.is_some()
                && self.failed_retirement.is_none()
                && self.inflight_retirement.is_none()
                && self.unstarted_retirement.is_none()
                && self.forget_failed.is_none(),
            "leaf archive lacks the original successful terminal Guardian",
        )?;
        require(
            monotonic_ns()? < self.native_deadline,
            "leaf archive exceeded original startup bound",
        )?;
        let creator = self
            .creator
            .as_ref()
            .ok_or_else(|| io::Error::other("leaf archive lacks original Creator"))?;
        require(
            creator.admitted && self.source_retired()?,
            "leaf archive source is not actually terminal and unlinked",
        )?;
        creator.check_terminal_snapshot(self.terminal_snapshot.as_ref().unwrap(), true)?;
        self.controls
            .as_ref()
            .ok_or_else(|| io::Error::other("leaf archive controls absent"))?
            .check()?;
        self.completed_queries()?;
        self.stop
            .as_ref()
            .ok_or_else(|| io::Error::other("leaf archive lacks original stop"))?
            .completed_custody(self.deadline)?;
        let history = self.journal.completed_leaf_history()?;
        let pairs: Vec<_> = history
            .frames
            .as_chunks::<2>().0.iter()
            .map(|frames| {
                json!({"intent_owner":frames[0].owner,"outcome_owner":frames[1].owner,
                "intent":frames[0].write,"outcome":frames[1].write,"line":frames[0].line})
            })
            .collect();
        let pairs = journal::canonical(&json!(pairs))?;
        self.check()?;
        require(
            monotonic_ns()? < self.native_deadline,
            "leaf archive completed after original startup bound",
        )?;
        Ok(LeafArchive {
            holder: self,
            history,
            pairs,
        })
    }
}
