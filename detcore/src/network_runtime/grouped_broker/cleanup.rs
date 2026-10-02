//! Creation-only failed-prefix cleanup. The old source Journals are never
//! repaired, imported as success, or used as removal journals. This module owns
//! the separate live-peer exchange and the only cleanup cursor epoch.
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use super::Failure;
use super::Intent;
use super::cleanup_native as ffi;
use super::guardian;
use super::journal;
use super::owner;
use super::require;
use super::wire;

// The separately preserved creation-only qualifier owns its generated build
// closure. Product startup is the authenticated entry/controller.rs path.
pub(super) mod startup;

/// No serialized constructor, Clone, numeric issuer or conversion from S1.
/// Only the closed entry below, whose source closure has no Provider path,
/// constructs this before retaining any child or source-write authorization.
#[derive(Debug)]
struct NoProviderCreated {
    _closed_entry: (),
}

#[derive(Debug)]
pub(super) struct Preparation {
    descriptors: [i32; 3],
    nonce: String,
    incarnation: u64,
    role: guardian::Role,
    stage: u64,
}
impl Preparation {
    fn local(
        controls: &owner::Controls,
        intent: &Intent,
        role: guardian::Role,
        stage: u64,
        ledger: &mut journal::RemovalJournal,
    ) -> io::Result<Self> {
        controls.check()?;
        ledger.store.verify()?;
        require(
            guardian::monotonic_ns()? < stage,
            "cleanup preparation original stage expired",
        )?;
        Ok(Self {
            descriptors: control_fds(controls)?,
            nonce: intent.nonce.clone(),
            incarnation: intent.incarnation,
            role,
            stage,
        })
    }
    pub(super) fn check(
        self,
        controls: &owner::Controls,
        intent: &Intent,
        role: guardian::Role,
        stage: u64,
    ) -> io::Result<()> {
        require(
            self.descriptors == control_fds(controls)?
                && self.nonce == intent.nonce
                && self.incarnation == intent.incarnation
                && self.role == role
                && self.stage == stage
                && guardian::monotonic_ns()? < stage,
            "cleanup preparation replaced original role/controls/deadline",
        )
    }
}
fn control_fds(controls: &owner::Controls) -> io::Result<[i32; 3]> {
    require(
        controls.fds.len() == 3,
        "cleanup requires original atomic three controls",
    )?;
    Ok([
        controls.fds[0].as_raw_fd(),
        controls.fds[1].as_raw_fd(),
        controls.fds[2].as_raw_fd(),
    ])
}
fn same_descriptions(original: [i32; 3], received: &[OwnedFd]) -> io::Result<()> {
    require(
        received.len() == 3,
        "cleanup control duplicate population differs",
    )?;
    for (index, fd) in received.iter().enumerate() {
        let raw = unsafe {
            libc::syscall(
                libc::SYS_kcmp,
                libc::getpid(),
                libc::getpid(),
                0,
                original[index] as libc::c_ulong,
                fd.as_raw_fd() as libc::c_ulong,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        require(
            raw == 0,
            "cleanup control duplicates are not the same actual OFDs",
        )?;
    }
    Ok(())
}
#[derive(Debug)]
pub(super) struct Readiness {
    intent: Intent,
    stage: u64,
    descriptors: [i32; 3],
    aliases: Vec<OwnedFd>,
    peer: owner::LauncherLease,
}
impl Readiness {
    pub(super) fn check(
        &self,
        controls: &owner::Controls,
        intent: &Intent,
        stage: u64,
    ) -> io::Result<()> {
        require(
            self.intent.nonce == intent.nonce
                && self.intent.incarnation == intent.incarnation
                && self.stage == stage
                && guardian::monotonic_ns()? < stage
                && self.descriptors == control_fds(controls)?,
            "cleanup peer readiness changed source identity",
        )?;
        self.peer.check_live()?;
        controls.check()?;
        same_descriptions(self.descriptors, &self.aliases)
    }
    fn live(&self) -> io::Result<()> {
        self.peer.check_live()?;
        same_descriptions(self.descriptors, &self.aliases)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CursorPhase {
    Source,
    Parent,
    #[expect(dead_code, reason = "Retained typed creation-recovery protocol is not yet wired into the startup continuation")]
    GrantSubmitted,
    Peer,
    #[expect(dead_code, reason = "Retained typed creation-recovery protocol is not yet wired into the startup continuation")]
    Unknown,
    Closed,
}
#[derive(Debug)]
struct Cursor {
    phase: CursorPhase,
    epoch: u64,
}
impl Cursor {
    fn retained() -> Self {
        Self {
            phase: CursorPhase::Source,
            epoch: 0,
        }
    }
    fn parent(&self) -> io::Result<()> {
        require(
            self.phase == CursorPhase::Parent,
            "cleanup cursor is not owned by C caller",
        )
    }
}
/// Borrow of the original Keeper controls for one actual granted epoch. No raw
/// FD constructor and no conversion from the successful S1 read capability.
pub(super) struct ReadEpoch<'a> {
    controls: &'a owner::Controls,
    cursor: &'a Cursor,
    epoch: u64,
    deadline: Instant,
    native_cutoff: u64,
    parent: BorrowedFd<'a>,
}
impl ReadEpoch<'_> {
    pub(super) fn check(&self, controls: &owner::Controls, deadline: Instant) -> io::Result<()> {
        require(
            std::ptr::eq(self.controls, controls)
                && self.cursor.phase == CursorPhase::Peer
                && self.cursor.epoch == self.epoch
                && self.epoch == 1
                && deadline <= self.deadline
                && Instant::now() < deadline
                && guardian::monotonic_ns()? < self.native_cutoff,
            "cleanup read lacks the original exclusive epoch",
        )?;
        require(
            !owner::terminal(self.parent.as_raw_fd())?,
            "cleanup cursor parent is terminal",
        )
    }
    fn snapshot(&mut self, index: usize) -> io::Result<Vec<u8>> {
        let controls = self.controls;
        let deadline = self.deadline;
        controls.snapshot_for_cleanup(self, index, deadline)
    }
}
#[derive(Debug)]
struct Selection {
    steps: Vec<ffi::RecoveryStep>,
    eligible: u32,
    relation: &'static str,
}
fn select_histories(
    intent: &Intent,
    guardian: &journal::SourceHistory,
    keeper: &journal::SourceHistory,
) -> io::Result<Selection> {
    let a = &guardian.frames;
    let b = &keeper.frames;
    require(
        a.len() >= b.len() && a.len() - b.len() <= 1,
        "Keeper ahead or source histories differ by more than one actual callback",
    )?;
    require(
        a[..b.len()] == b[..],
        "actual source history common frames disagree",
    )?;
    let (selected, relation) = if a.len() == b.len() {
        (a.as_slice(), "equal-prefix")
    } else if a.last().unwrap().write.started == 0 {
        (b.as_slice(), "guardian-only-intent-excluded")
    } else {
        (a.as_slice(), "guardian-only-outcome-retained")
    };
    require(
        !selected.is_empty(),
        "cleanup has no jointly acknowledged eligible source attempt",
    )?;
    let mut steps = Vec::new();
    for pair in selected.chunks(2) {
        let first = &pair[0];
        let line = intent.command(first.write.role, 0)?;
        let mut step = ffi::RecoveryStep::default();
        step.pair.intent_owner = ffi_owner(&first.owner)?;
        step.pair.intent = ffi_write(&first.write)?;
        require(
            line.len() < step.pair.line.len(),
            "creation line exceeds maintained256 bound",
        )?;
        for (to, from) in step.pair.line.iter_mut().zip(line.bytes()) {
            *to = from as libc::c_char;
        }
        if pair.len() == 2 {
            step.outcome_present = 1;
            step.pair.outcome_owner = ffi_owner(&pair[1].owner)?;
            step.pair.outcome = ffi_write(&pair[1].write)?;
        }
        steps.push(step);
    }
    require(
        steps.len() <= 17,
        "cleanup selection exceeds original17 sites",
    )?;
    Ok(Selection {
        eligible: (1u32 << steps.len()) - 1,
        steps,
        relation,
    })
}
/// Descriptive selection only. Callers must independently establish actual
/// terminal custody, healthy mirrored Stores and exclusive native cleanup.
pub(super) fn select_runtime_creation_recovery(
    intent: &Intent,
    guardian: &journal::SourceHistory,
    keeper: &journal::SourceHistory,
) -> io::Result<(Vec<ffi::RecoveryStep>, u32, &'static str)> {
    let selected = select_histories(intent, guardian, keeper)?;
    Ok((selected.steps, selected.eligible, selected.relation))
}
fn ffi_owner(value: &journal::OwnerSnapshot) -> io::Result<ffi::Owner> {
    let mut out = ffi::Owner {
        incarnation: value.incarnation,
        phase: value.phase as _,
        verified_sites: value.verified_sites,
        attempted_sites: value.attempted_sites,
        event_id: value.event_id,
        write_unknown: value.write_unknown,
        pending_role: value.pending_role,
        pending_remove: value.pending_remove,
        pending_bytes: usize::try_from(value.pending_bytes).map_err(io::Error::other)?,
        ..ffi::Owner::default()
    };
    require(
        value.group.len() < out.group.len() && value.event.len() < out.event.len(),
        "cleanup owner names exceed native bounds",
    )?;
    for (to, from) in out.group.iter_mut().zip(value.group.bytes()) {
        *to = from as libc::c_char;
    }
    for (to, from) in out.event.iter_mut().zip(value.event.bytes()) {
        *to = from as libc::c_char;
    }
    Ok(out)
}
fn ffi_write(value: &journal::Write) -> io::Result<ffi::Write> {
    Ok(ffi::Write {
        role: value.role,
        remove: value.remove,
        submitted: usize::try_from(value.submitted).map_err(io::Error::other)?,
        raw: isize::try_from(value.raw).map_err(io::Error::other)?,
        error: value.error as libc::c_int,
        started: value.started as libc::c_int,
        completed: value.completed as libc::c_int,
    })
}
fn rust_owner(value: &ffi::Owner) -> io::Result<journal::OwnerSnapshot> {
    fn name(bytes: &[libc::c_char]) -> io::Result<String> {
        let end = bytes
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| io::Error::other("native name lacks NUL"))?;
        require(
            bytes[end..].iter().all(|b| *b == 0),
            "native name has trailing bytes",
        )?;
        String::from_utf8(bytes[..end].iter().map(|b| *b as u8).collect()).map_err(io::Error::other)
    }
    Ok(journal::OwnerSnapshot {
        incarnation: value.incarnation,
        phase: value.phase,
        verified_sites: value.verified_sites,
        attempted_sites: value.attempted_sites,
        event_id: value.event_id,
        write_unknown: value.write_unknown,
        pending_role: value.pending_role,
        pending_remove: value.pending_remove,
        pending_bytes: value.pending_bytes as u64,
        group: name(&value.group)?,
        event: name(&value.event)?,
    })
}
fn rust_write(value: &ffi::Write) -> io::Result<journal::Write> {
    require(
        value.error >= 0 && value.started >= 0 && value.completed >= 0,
        "native write unsigned conversion differs",
    )?;
    Ok(journal::Write {
        role: value.role,
        remove: value.remove,
        submitted: value.submitted as u64,
        raw: value.raw as i64,
        error: value.error as u32,
        started: value.started as u32,
        completed: value.completed as u32,
    })
}
/// Two actual healthy ledgers and original native custody were joined before
/// issuance. This is deliberately distinct from Guardian.failed_agreement.
#[derive(Debug)]
pub(super) struct AsymmetricAgreement {
    guardian: Value,
    keeper: Value,
    #[expect(dead_code, reason = "Retained typed creation-recovery protocol is not yet wired into the startup continuation")]
    guardian_history: journal::SourceHistory,
    keeper_history: journal::SourceHistory,
    selection: Selection,
    deadline: Instant,
    native_cutoff: u64,
    digest: [u8; 32],
    peer_acknowledged: bool,
}
fn terminal_identity(a: &Value, b: &Value) -> io::Result<()> {
    require(
        a.as_object()
            .is_some_and(|a| b.as_object().is_some_and(|b| a.keys().eq(b.keys()))),
        "creation terminal field sets differ",
    )?;
    for key in [
        "schema",
        "native_origin",
        "creator",
        "creator_admitted",
        "actual_terminal_manager",
        "source_eof",
        "source_retired",
        "controls_held",
        "history_complete_claimed",
        "provider_authority_created",
    ] {
        require(
            a.get(key).is_some() && a[key] == b[key],
            "creation terminal native identity or scope differs",
        )?;
    }
    require(
        a["schema"] == "hermit-creation-prefix-terminal-v1"
            && a["creator_admitted"] == true
            && a["controls_held"] == 3
            && a["source_eof"] == true
            && a["source_retired"] == true
            && a["history_complete_claimed"] == false
            && a["provider_authority_created"] == false,
        "creation terminal report lacks original partial native custody",
    )?;
    for value in [a, b] {
        require(
            value["original_failure"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "creation terminal original failure absent",
        )?;
    }
    Ok(())
}
impl AsymmetricAgreement {
    fn issue(
        intent: &Intent,
        guardian: Value,
        keeper: Value,
        g: journal::SourceHistory,
        k: journal::SourceHistory,
        deadline: Instant,
        native_cutoff: u64,
    ) -> io::Result<Self> {
        let selection = select_histories(intent, &g, &k)?;
        Self::joined(guardian, keeper, g, k, selection, deadline, native_cutoff)
    }
    fn issue_no_writes(
        guardian: Value,
        keeper: Value,
        g: journal::SourceHistory,
        k: journal::SourceHistory,
        deadline: Instant,
        native_cutoff: u64,
    ) -> io::Result<Self> {
        require(
            g.frames.len() == 1
                && k.frames.is_empty()
                && g.frames[0].sequence == 1
                && g.frames[0].write.role == 1
                && g.frames[0].write.started == 0
                && g.frames[0].write.completed == 0,
            "zero-write agreement lacks actual first-intent-only histories",
        )?;
        let selection = Selection {
            steps: Vec::new(),
            eligible: 0,
            relation: "guardian-first-intent-not-acknowledged-no-writes",
        };
        Self::joined(guardian, keeper, g, k, selection, deadline, native_cutoff)
    }
    fn joined(
        guardian: Value,
        keeper: Value,
        g: journal::SourceHistory,
        k: journal::SourceHistory,
        selection: Selection,
        deadline: Instant,
        native_cutoff: u64,
    ) -> io::Result<Self> {
        terminal_identity(&guardian, &keeper)?;
        let native_cutoff = native_cutoff
            .min(
                guardian["cleanup_cutoff"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("Guardian original cutoff absent"))?,
            )
            .min(
                keeper["cleanup_cutoff"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("Keeper original cutoff absent"))?,
            );
        let now = guardian::monotonic_ns()?;
        require(
            now < native_cutoff,
            "dual-history earliest original cutoff expired",
        )?;
        let deadline = deadline.min(Instant::now() + Duration::from_nanos(native_cutoff - now));
        require(
            guardian["source_store"] == g.commitment() && keeper["source_store"] == k.commitment(),
            "creation agreement replaced actual healthy bytes",
        )?;
        let record = json!({"schema":"hermit-creation-cleanup-agreement-v1","guardian":guardian,"keeper":keeper,
            "eligible":selection.eligible,"relation":selection.relation,"cutoff":native_cutoff});
        let digest = Sha256::digest(journal::canonical(&record)?).into();
        Ok(Self {
            guardian,
            keeper,
            guardian_history: g,
            keeper_history: k,
            selection,
            deadline,
            native_cutoff,
            digest,
            peer_acknowledged: false,
        })
    }
    fn packet(&self) -> Value {
        json!({"schema":"hermit-creation-cleanup-agreement-v1","guardian":self.guardian,"keeper":self.keeper,
        "eligible":self.selection.eligible,"relation":self.selection.relation,"cutoff":self.native_cutoff})
    }
    #[expect(dead_code, reason = "Retained typed creation-recovery protocol is not yet wired into the startup continuation")]
    pub(super) fn check_holder(&self, record: &Value, deadline: Instant) -> io::Result<()> {
        require(
            self.peer_acknowledged
                && record == &self.guardian
                && deadline >= self.deadline
                && Instant::now() < self.deadline
                && guardian::monotonic_ns()? < self.native_cutoff,
            "creation cleanup agreement lacks original live-peer ACK or deadline",
        )
    }
}
#[derive(Debug)]
struct CallbackOwner {
    intent: Intent,
    parent: wire::Channel,
    peer: Option<wire::Credentials>,
    ready: Option<Readiness>,
    peer_ledger: Option<journal::SourceLedgerReader>,
    ledger: journal::RemovalJournal,
    cursor: Cursor,
    agreement: Option<AsymmetricAgreement>,
    stage: Instant,
    native_stage: u64,
    deadline: Option<Instant>,
    native_cutoff: Option<u64>,
    release: Option<ffi::Release>,
    primary: Option<Failure>,
    callback_failure: Option<Failure>,
    closed: bool,
    removal_sequence: u64,
}
impl CallbackOwner {
    fn cutoff(&self) -> io::Result<(Instant, u64)> {
        let d = self
            .deadline
            .ok_or_else(|| io::Error::other("cleanup original cutoff absent"))?;
        let n = self
            .native_cutoff
            .ok_or_else(|| io::Error::other("cleanup native cutoff absent"))?;
        require(
            Instant::now() < d && guardian::monotonic_ns()? < n,
            "cleanup original cutoff expired",
        )?;
        self.ready
            .as_ref()
            .ok_or_else(|| io::Error::other("cleanup live peer absent"))?
            .live()?;
        Ok((d, n))
    }
    fn send(&mut self, value: Value) -> io::Result<()> {
        self.cutoff()?;
        let bytes = journal::canonical(&value)?;
        require(
            bytes.len() <= 4096,
            "cleanup packet exceeds unchanged4096 bound",
        )?;
        self.parent.send_once(&bytes, &[])?;
        self.cutoff()?;
        Ok(())
    }
    fn receive(&mut self) -> io::Result<Value> {
        loop {
            self.cutoff()?;
            if let Some(index) = self.parent.receive(4096)? {
                let packet = &self.parent.packets[index];
                packet.exact(
                    0,
                    self.peer.ok_or_else(|| {
                        io::Error::other("original cleanup Keeper credentials absent")
                    })?,
                )?;
                let value: Value = serde_json::from_slice(&packet.bytes)?;
                require(
                    journal::canonical(&value)? == packet.bytes,
                    "cleanup peer packet is not canonical",
                )?;
                self.cutoff()?;
                return Ok(value);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn remember_callback(&mut self, result: io::Result<()>) -> libc::c_int {
        match result {
            Ok(()) => 0,
            Err(error) => {
                self.callback_failure
                    .get_or_insert_with(|| Failure::capture(&error));
                unsafe {
                    *libc::__errno_location() = error.raw_os_error().unwrap_or(libc::EPROTO);
                }
                -1
            }
        }
    }
    #[expect(dead_code, reason = "Retained typed creation-recovery protocol is not yet wired into the startup continuation")]
    fn ack_recovery(
        &mut self,
        native: &ffi::Owner,
        descriptors: *const libc::c_int,
        steps: *const ffi::RecoveryStep,
        count: usize,
    ) -> io::Result<()> {
        self.cutoff()?;
        self.cursor.parent()?;
        let agreement = self.agreement.as_ref().ok_or_else(|| {
            io::Error::other("native recovery lacks actual dual-history agreement")
        })?;
        require(
            agreement.peer_acknowledged
                && count == agreement.selection.steps.len()
                && count > 0
                && native.attempted_sites == agreement.selection.eligible,
            "native recovery changed eligible source attempts",
        )?;
        require(
            !steps.is_null() && !descriptors.is_null(),
            "native recovery omitted actual inputs",
        )?;
        require(
            unsafe { std::slice::from_raw_parts(steps, count) }
                == agreement.selection.steps.as_slice(),
            "native recovery changed actual selected original steps",
        )?;
        let actual = unsafe { std::slice::from_raw_parts(descriptors, 3) };
        for (index, fd) in actual.iter().enumerate() {
            let raw = unsafe {
                libc::syscall(
                    libc::SYS_kcmp,
                    libc::getpid(),
                    libc::getpid(),
                    0,
                    *fd as libc::c_ulong,
                    self.ready.as_ref().unwrap().descriptors[index] as libc::c_ulong,
                )
            };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            require(raw == 0, "native recovery changed actual OFD")?;
        }
        // C calls synchronously while its reads are paused. Mark ambiguous
        // ownership BEFORE send: any error permanently prevents local reads.
        require(self.cursor.epoch == 0, "cleanup read grant cannot repeat")?;
        self.cursor.epoch = 1;
        self.cursor.phase = CursorPhase::GrantSubmitted;
        let digest = super::hex(&agreement.digest);
        let eligible = agreement.selection.eligible;
        let request = json!({"schema":"hermit-cleanup-read-grant-v1","nonce":self.intent.nonce,
            "incarnation":self.intent.incarnation,"epoch":1,"agreement":digest,"eligible":eligible,
            "cutoff":self.native_cutoff.unwrap()});
        if let Err(error) = self.send(request) {
            self.cursor.phase = CursorPhase::Unknown;
            return Err(error);
        }
        self.cursor.phase = CursorPhase::Peer;
        let result = (|| {
            let response = self.receive()?;
            require(
                response
                    == json!({"schema":"hermit-cleanup-read-done-v1","nonce":self.intent.nonce,
                "incarnation":self.intent.incarnation,"epoch":1,"agreement":digest,"cutoff":self.native_cutoff.unwrap()}),
                "cleanup peer read relinquishment differs",
            )?;
            self.ledger
                .store
                .append(json!({"kind":"actual-peer-read-relinquished","value":response}))?;
            self.cutoff()?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.cursor.phase = CursorPhase::Parent;
                Ok(())
            }
            Err(error) => {
                self.cursor.phase = CursorPhase::Unknown;
                Err(error)
            }
        }
    }
    fn journal_remove(
        &mut self,
        owner: &ffi::Owner,
        write: &ffi::Write,
        line: *const libc::c_char,
    ) -> io::Result<()> {
        self.cutoff()?;
        self.cursor.parent()?;
        require(
            !line.is_null() && write.submitted < 256,
            "native removal callback span differs",
        )?;
        let bytes = unsafe { std::slice::from_raw_parts(line.cast::<u8>(), write.submitted) };
        let o = rust_owner(owner)?;
        let w = rust_write(write)?;
        let digest = self.ledger.append(o.clone(), w.clone(), bytes)?;
        self.removal_sequence += 1;
        let sequence = self.removal_sequence;
        let request = json!({"schema":"hermit-cleanup-remove-v1","nonce":self.intent.nonce,"incarnation":self.intent.incarnation,
            "sequence":sequence,"owner":o,"write":w,"line":super::hex(bytes),"cutoff":self.native_cutoff.unwrap()});
        self.send(request)?;
        let response = self.receive()?;
        require(
            response
                == json!({"schema":"hermit-cleanup-remove-ack-v1","nonce":self.intent.nonce,
            "incarnation":self.intent.incarnation,"sequence":sequence,"cutoff":self.native_cutoff.unwrap()}),
            "cleanup removal peer ACK differs",
        )?;
        self.ledger.store.append(json!({"kind":"dual-removal-ack","sequence":sequence,"local_digest":super::hex(&digest),"peer":response}))?;
        self.cutoff()?;
        Ok(())
    }
}
#[expect(dead_code, reason = "Retained typed creation-recovery protocol is not yet wired into the startup continuation")]
unsafe extern "C" fn recovery_callback(
    context: *mut libc::c_void,
    owner: *const ffi::Owner,
    descriptors: *const libc::c_int,
    steps: *const ffi::RecoveryStep,
    count: usize,
) -> libc::c_int {
    if context.is_null() || owner.is_null() {
        unsafe {
            *libc::__errno_location() = libc::EINVAL;
        }
        return -1;
    }
    let state = unsafe { &mut *context.cast::<CallbackOwner>() };
    // No unwinding may cross the C boundary. An actual panic is a refusal and
    // cannot return cursor ownership; the retained process-level supervisor
    // remains responsible for that failed owner.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.ack_recovery(unsafe { &*owner }, descriptors, steps, count)
    }))
    .unwrap_or_else(|_| {
        state.cursor.phase = CursorPhase::Unknown;
        Err(io::Error::other("cleanup recovery callback panicked"))
    });
    state.remember_callback(result)
}
unsafe extern "C" fn removal_callback(
    context: *mut libc::c_void,
    owner: *const ffi::Owner,
    write: *const ffi::Write,
    line: *const libc::c_char,
) -> libc::c_int {
    if context.is_null() || owner.is_null() || write.is_null() {
        unsafe {
            *libc::__errno_location() = libc::EINVAL;
        }
        return -1;
    }
    let state = unsafe { &mut *context.cast::<CallbackOwner>() };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.journal_remove(unsafe { &*owner }, unsafe { &*write }, line)
    }))
    .unwrap_or_else(|_| Err(io::Error::other("cleanup removal callback panicked")));
    state.remember_callback(result)
}

#[derive(Debug)]
struct CleanupEnvelope {
    no_provider: NoProviderCreated,
    holder: guardian::Holder,
    bridge: ffi::CleanupBridge,
    callbacks: Box<CallbackOwner>,
    source: Option<owner::Launcher>,
    keeper: Option<owner::Launcher>,
    source_logs: OwnedFd,
    keeper_logs: OwnedFd,
    registration_attempted: bool,
    prepared: bool,
    inventory: owner::CensusInventory,
    #[expect(dead_code, reason = "Retained typed creation-recovery protocol is not yet wired into the startup continuation")]
    recovery_steps: [ffi::RecoveryStep; 17],
}
impl CleanupEnvelope {
    fn prepare_before_source(&mut self) -> io::Result<()> {
        self.callbacks.ledger.initialize("guardian")?;
        unsafe {
            self.bridge.initialize()?;
        }
        self.holder.install_creation_cleanup()?;
        self.holder.initialize()
    }
    fn prepare_controls(&mut self) -> io::Result<()> {
        require(
            !self.prepared && !self.registration_attempted,
            "cleanup original preparation repeated",
        )?;
        let controls = self.holder.creation_controls()?;
        let fds = [
            controls.fds[0].as_fd(),
            controls.fds[1].as_fd(),
            controls.fds[2].as_fd(),
        ];
        let context = (&mut *self.callbacks as *mut CallbackOwner).cast();
        unsafe {
            self.bridge.prepare(
                self.callbacks.intent.incarnation,
                &self.callbacks.intent.nonce,
                fds,
                self.callbacks.native_stage,
                removal_callback,
                context,
            )?;
        }
        let proof = Preparation::local(
            controls,
            &self.callbacks.intent,
            guardian::Role::Guardian,
            self.callbacks.native_stage,
            &mut self.callbacks.ledger,
        )?;
        let record = json!({"schema":"hermit-cleanup-guardian-ready-v1","nonce":self.callbacks.intent.nonce,
            "incarnation":self.callbacks.intent.incarnation,"stage_deadline":self.callbacks.native_stage});
        self.callbacks
            .ledger
            .store
            .append(json!({"kind":"guardian-cleanup-prepared","value":record}))?;
        self.registration_attempted = true;
        let store = self.holder.creation_source_rights()?;
        self.callbacks.parent.send_creation_bundle(
            &journal::canonical(&record)?,
            wire::CreationLedgerBundle::new(fds, store),
        )?;
        self.holder.install_creation_preparation(proof)?;
        self.prepared = true;
        Ok(())
    }
    fn receive_readiness(&mut self) -> io::Result<bool> {
        require(
            self.prepared && self.callbacks.ready.is_none(),
            "cleanup peer readiness repeated or before local preparation",
        )?;
        let Some(index) = self.callbacks.parent.receive(4096)? else {
            return Ok(false);
        };
        let packet = &mut self.callbacks.parent.packets[index];
        packet.exact(
            5,
            self.callbacks
                .peer
                .ok_or_else(|| io::Error::other("original Keeper credentials absent"))?,
        )?;
        require(
            packet.bytes
                == journal::canonical(&json!({"schema":"hermit-cleanup-keeper-ready-v1",
            "nonce":self.callbacks.intent.nonce,"incarnation":self.callbacks.intent.incarnation,
            "stage_deadline":self.callbacks.native_stage}))?,
            "cleanup Keeper readiness identity differs",
        )?;
        let mut rights = std::mem::take(&mut packet.rights);
        let ledger = rights.split_off(3);
        self.callbacks.peer_ledger = Some(journal::SourceLedgerReader::retain(
            ledger,
            self.callbacks.intent.clone(),
        ));
        self.callbacks.ready = Some(Readiness {
            intent: self.callbacks.intent.clone(),
            stage: self.callbacks.native_stage,
            descriptors: control_fds(self.holder.creation_controls()?)?,
            aliases: rights,
            peer: self
                .keeper
                .as_ref()
                .ok_or_else(|| io::Error::other("original Keeper Launcher absent"))?
                .source_lease()?,
        });
        self.callbacks.peer_ledger.as_mut().unwrap().initialize()?;
        self.callbacks.ready.as_ref().unwrap().check(
            self.holder.creation_controls()?,
            &self.callbacks.intent,
            self.callbacks.native_stage,
        )?;
        self.callbacks.ledger.store.append(
            json!({"kind":"actual-live-keeper-prepared","pid":self.callbacks.peer.unwrap().pid}),
        )?;
        Ok(true)
    }
    #[expect(dead_code, reason = "Retained typed creation-recovery protocol is not yet wired into the startup continuation")]
    fn cancel_after_prefix(&mut self, cause: io::Error) -> io::Result<()> {
        require(
            self.callbacks.primary.is_none(),
            "creation cancellation cannot replace original failure",
        )?;
        self.callbacks.primary = Some(Failure::capture(&cause));
        // This Holder latches before SHUT_WR; supplied origin is the original
        // actual first failure, not a fresh release time sampled on retry.
        let origin = if self.holder.held_creation_sequence() == Some(1) {
            self.holder.cancel_creation_before_first_ack(&cause)?
        } else {
            self.holder.cancel_creation_after_prefix(&cause)?
        };
        let cutoff = origin
            .checked_add(1_000_000_000)
            .ok_or_else(|| io::Error::other("cleanup origin overflow"))?
            .min(self.callbacks.native_stage);
        let now = guardian::monotonic_ns()?;
        require(now < cutoff, "creation original release already expired")?;
        self.callbacks.native_cutoff = Some(cutoff);
        self.callbacks.deadline = Some(
            self.callbacks
                .stage
                .min(Instant::now() + Duration::from_nanos(cutoff - now)),
        );
        self.callbacks.release = Some(ffi::Release {
            release_start: origin,
            first_failure_origin: origin,
            enclosing_cutoff: cutoff,
            has_first_failure: 1,
        });
        self.callbacks.send(json!({"schema":"hermit-cleanup-cancel-v1","nonce":self.callbacks.intent.nonce,
            "incarnation":self.callbacks.intent.incarnation,"origin":origin,"cutoff":cutoff,"cause":cause.to_string()}))
    }
    #[expect(dead_code, reason = "Retained typed creation-recovery protocol is not yet wired into the startup continuation")]
    fn join_terminal_histories(&mut self) -> io::Result<()> {
        self.callbacks.cutoff()?;
        let peer = self.callbacks.receive()?;
        require(
            peer["schema"] == "hermit-creation-prefix-terminal-v1",
            "cleanup expected actual Keeper terminal report",
        )?;
        let own = self.holder.creation_terminal_record()?;
        let a = self.holder.creation_source_history()?;
        let b = self
            .callbacks
            .peer_ledger
            .as_mut()
            .ok_or_else(|| io::Error::other("original Keeper Store absent"))?
            .read(&peer["source_store"])?;
        let agreement = if a.frames.len() == 1 && b.frames.is_empty() {
            AsymmetricAgreement::issue_no_writes(
                own,
                peer,
                a,
                b,
                self.callbacks.deadline.unwrap(),
                self.callbacks.native_cutoff.unwrap(),
            )?
        } else {
            AsymmetricAgreement::issue(
                &self.callbacks.intent,
                own,
                peer,
                a,
                b,
                self.callbacks.deadline.unwrap(),
                self.callbacks.native_cutoff.unwrap(),
            )?
        };
        self.callbacks.native_cutoff = Some(agreement.native_cutoff);
        self.callbacks.deadline = Some(self.callbacks.deadline.unwrap().min(agreement.deadline));
        self.callbacks.release.as_mut().unwrap().enclosing_cutoff = agreement.native_cutoff;
        self.callbacks.agreement = Some(agreement);
        let record = self.callbacks.agreement.as_ref().unwrap().packet();
        self.callbacks
            .ledger
            .store
            .append(json!({"kind":"dual-source-history-agreement","value":record}))?;
        self.callbacks.send(record)?;
        let ack = self.callbacks.receive()?;
        let digest = super::hex(&self.callbacks.agreement.as_ref().unwrap().digest);
        require(
            ack == json!({"schema":"hermit-cleanup-history-ack-v1","nonce":self.callbacks.intent.nonce,
            "incarnation":self.callbacks.intent.incarnation,"agreement":digest,"cutoff":self.callbacks.native_cutoff.unwrap()}),
            "cleanup actual Keeper agreement ACK differs",
        )?;
        self.callbacks
            .ledger
            .store
            .append(json!({"kind":"dual-source-history-ack","value":ack}))?;
        self.callbacks.cutoff()?;
        self.callbacks.agreement.as_mut().unwrap().peer_acknowledged = true;
        self.callbacks.cursor.phase = CursorPhase::Parent;
        Ok(())
    }
    #[expect(dead_code, reason = "Retained typed creation-recovery protocol is not yet wired into the startup continuation")]
    fn delete_owned_prefix(&mut self) -> io::Result<()> {
        self.callbacks.cutoff()?;
        self.callbacks.cursor.parent()?;
        let agreement = self
            .callbacks
            .agreement
            .as_ref()
            .ok_or_else(|| io::Error::other("cleanup agreement absent"))?;
        agreement.check_holder(
            &self.holder.creation_terminal_record()?,
            self.callbacks.deadline.unwrap(),
        )?;
        require(
            !agreement.selection.steps.is_empty() && agreement.selection.eligible != 0,
            "zero-write agreement cannot authorize recovery adoption or deletion",
        )?;
        let source_proof = self
            .source
            .as_mut()
            .ok_or_else(|| io::Error::other("original source Launcher absent before adoption"))?
            .failed_source_terminal(
                self.source_logs.as_raw_fd(),
                self.callbacks.deadline.unwrap(),
            )?
            .ok_or_else(|| {
                io::Error::other("original source Launcher not terminal before adoption")
            })?;
        self.callbacks.ledger.store.append(json!({"kind":"original-source-launcher-terminal-before-adoption","value":source_proof.observation()?}))?;
        source_proof.check()?;
        let count = self
            .callbacks
            .agreement
            .as_ref()
            .unwrap()
            .selection
            .steps
            .len();
        self.recovery_steps[..count]
            .copy_from_slice(&self.callbacks.agreement.as_ref().unwrap().selection.steps);
        // FFI's input slice lives outside CallbackOwner. The synchronous callback
        // may mutably borrow that entire Box without aliasing a Rust input slice.
        let context = (&mut *self.callbacks as *mut CallbackOwner).cast();
        unsafe {
            self.bridge.adopt(
                &self.recovery_steps[..count],
                recovery_callback,
                context,
                self.callbacks.release.unwrap(),
            )?;
        }
        self.callbacks.cutoff()?;
        self.callbacks.cursor.parent()?;
        unsafe {
            self.bridge.delete()?;
        }
        self.callbacks.ledger.complete()?;
        for _ in 0..2 {
            self.callbacks.cutoff()?;
            self.callbacks.cursor.parent()?;
            unsafe {
                self.bridge.observe_absent()?;
            }
        }
        self.callbacks.cutoff()?;
        unsafe {
            self.bridge.release_aliases()?;
        }
        self.callbacks.ledger.store.append(json!({"kind":"two-fresh-C-absence-receipts-and-alias-retirement","original_cutoff":self.callbacks.native_cutoff}))?;
        Ok(())
    }
    #[expect(dead_code, reason = "Retained typed creation-recovery protocol is not yet wired into the startup continuation")]
    fn close_peer(&mut self) -> io::Result<()> {
        self.callbacks.cutoff()?;
        self.callbacks.send(json!({"schema":"hermit-cleanup-close-v1","nonce":self.callbacks.intent.nonce,
            "incarnation":self.callbacks.intent.incarnation,"agreement":super::hex(&self.callbacks.agreement.as_ref().unwrap().digest),
            "cutoff":self.callbacks.native_cutoff.unwrap()}))?;
        self.callbacks.cursor.phase = CursorPhase::Closed;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PeerPhase {
    Preparing,
    Source,
    Retiring,
    Reported,
    Agreed,
    Relinquished,
    Closed,
}
#[derive(Debug)]
pub(super) struct CreationPeer {
    intent: Intent,
    ledger: journal::RemovalJournal,
    aliases: Vec<OwnedFd>,
    guardian_ledger: Option<journal::SourceLedgerReader>,
    phase: PeerPhase,
    stage: Instant,
    native_stage: u64,
    deadline: Option<Instant>,
    cutoff: Option<u64>,
    original_cause: Option<String>,
    original_origin: Option<u64>,
    agreement: Option<AsymmetricAgreement>,
    cursor: Cursor,
    read_receipts: Option<[Vec<u8>; 2]>,
    next_removal: u64,
    refused: Option<Failure>,
}
impl CreationPeer {
    pub(super) fn retain(
        directory: OwnedFd,
        intent: Intent,
        stage: Instant,
        native_stage: u64,
    ) -> Self {
        Self {
            ledger: journal::RemovalJournal::retain(directory, intent.clone()),
            intent,
            aliases: Vec::new(),
            guardian_ledger: None,
            phase: PeerPhase::Preparing,
            stage,
            native_stage,
            deadline: None,
            cutoff: None,
            original_cause: None,
            original_origin: None,
            agreement: None,
            cursor: Cursor::retained(),
            read_receipts: None,
            next_removal: 1,
            refused: None,
        }
    }
    pub(super) fn initialize(&mut self) -> io::Result<()> {
        self.ledger.initialize("keeper")
    }
    fn check(&self, parent: BorrowedFd<'_>) -> io::Result<()> {
        if let Some(error) = &self.refused {
            return Err(error.error());
        }
        require(
            Instant::now() < self.deadline.unwrap_or(self.stage)
                && guardian::monotonic_ns()? < self.cutoff.unwrap_or(self.native_stage),
            "original live cleanup peer cutoff expired",
        )?;
        require(
            !owner::terminal(parent.as_raw_fd())?,
            "original cleanup parent is terminal",
        )
    }
    fn receive(
        &self,
        parent: &mut wire::Channel,
        credentials: wire::Credentials,
        pidfd: BorrowedFd<'_>,
        rights: usize,
    ) -> io::Result<Option<usize>> {
        self.check(pidfd)?;
        let Some(index) = parent.receive(4096)? else {
            return Ok(None);
        };
        parent.packets[index].exact(rights, credentials)?;
        self.check(pidfd)?;
        Ok(Some(index))
    }
    fn send(
        &mut self,
        parent: &mut wire::Channel,
        pidfd: BorrowedFd<'_>,
        value: Value,
    ) -> io::Result<()> {
        self.check(pidfd)?;
        let bytes = journal::canonical(&value)?;
        require(
            bytes.len() <= 4096,
            "cleanup live peer packet exceeds original4096",
        )?;
        parent.send_once(&bytes, &[])?;
        self.check(pidfd)
    }
    pub(super) fn progress(
        &mut self,
        holder: &mut guardian::Holder,
        parent: &mut wire::Channel,
        credentials: wire::Credentials,
        pidfd: BorrowedFd<'_>,
    ) -> io::Result<bool> {
        let result = self.progress_inner(holder, parent, credentials, pidfd);
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn progress_inner(
        &mut self,
        holder: &mut guardian::Holder,
        parent: &mut wire::Channel,
        credentials: wire::Credentials,
        pidfd: BorrowedFd<'_>,
    ) -> io::Result<bool> {
        self.check(pidfd)?;
        match self.phase {
            PeerPhase::Preparing => {
                if !holder.awaiting_creation_preparation() {
                    return Ok(false);
                }
                let Some(index) = self.receive(parent, credentials, pidfd, 5)? else {
                    return Ok(false);
                };
                let packet = &mut parent.packets[index];
                require(
                    packet.bytes
                        == journal::canonical(
                            &json!({"schema":"hermit-cleanup-guardian-ready-v1",
                    "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,"stage_deadline":self.native_stage}),
                        )?,
                    "cleanup actual Guardian preparation packet differs",
                )?;
                self.aliases = std::mem::take(&mut packet.rights);
                let rights = self.aliases.split_off(3);
                self.guardian_ledger = Some(journal::SourceLedgerReader::retain(
                    rights,
                    self.intent.clone(),
                ));
                self.guardian_ledger.as_mut().unwrap().initialize()?;
                let controls = holder.creation_controls()?;
                same_descriptions(control_fds(controls)?, &self.aliases)?;
                let proof = Preparation::local(
                    controls,
                    &self.intent,
                    guardian::Role::Keeper,
                    self.native_stage,
                    &mut self.ledger,
                )?;
                let record = json!({"schema":"hermit-cleanup-keeper-ready-v1","nonce":self.intent.nonce,
                    "incarnation":self.intent.incarnation,"stage_deadline":self.native_stage});
                self.ledger
                    .store
                    .append(json!({"kind":"actual-live-guardian-prepared","value":record}))?;
                let store = holder.creation_source_rights()?;
                // Phase is occupied before this irreversible send. No retry can
                // resend readiness or replace the original five descriptions.
                self.phase = PeerPhase::Source;
                parent.send_creation_bundle(
                    &journal::canonical(&record)?,
                    wire::CreationLedgerBundle::new(
                        [
                            controls.fds[0].as_fd(),
                            controls.fds[1].as_fd(),
                            controls.fds[2].as_fd(),
                        ],
                        store,
                    ),
                )?;
                self.check(pidfd)?;
                holder.install_creation_preparation(proof)?;
                Ok(true)
            }
            PeerPhase::Source => {
                let Some(index) = self.receive(parent, credentials, pidfd, 0)? else {
                    return Ok(false);
                };
                let bytes = &parent.packets[index].bytes;
                let value: Value = serde_json::from_slice(bytes)?;
                let origin = value["origin"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("cleanup cancellation origin absent"))?;
                let cutoff = value["cutoff"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("cleanup cancellation cutoff absent"))?;
                let cause = value["cause"]
                    .as_str()
                    .ok_or_else(|| io::Error::other("cleanup cancellation cause absent"))?
                    .to_owned();
                require(
                    !cause.is_empty()
                        && *bytes
                            == journal::canonical(
                                &json!({"schema":"hermit-cleanup-cancel-v1","nonce":self.intent.nonce,
                    "incarnation":self.intent.incarnation,"origin":origin,"cutoff":cutoff,"cause":cause}),
                            )?,
                    "cleanup cancellation changed original run or fields",
                )?;
                self.original_cause = Some(cause.clone());
                self.original_origin = Some(origin);
                self.phase = PeerPhase::Retiring;
                holder.cancel_creation_peer(&io::Error::other(cause), origin)?;
                let bound = holder
                    .creation_original_cutoff()?
                    .min(cutoff)
                    .min(self.native_stage);
                let now = guardian::monotonic_ns()?;
                require(
                    origin <= now
                        && now < bound
                        && bound
                            <= origin
                                .checked_add(1_000_000_000)
                                .ok_or_else(|| io::Error::other("cleanup origin overflow"))?,
                    "cleanup original cancellation bound differs",
                )?;
                self.cutoff = Some(bound);
                self.deadline = Some(
                    self.stage
                        .min(Instant::now() + Duration::from_nanos(bound - now)),
                );
                self.ledger.store.append(json!({"kind":"source-operation-failed","origin":origin,"cutoff":bound,"cause":self.original_cause}))?;
                Ok(true)
            }
            PeerPhase::Retiring => {
                if !holder.progress_failed_retirement()? {
                    return Ok(false);
                }
                let record = holder.creation_terminal_record()?;
                self.ledger
                    .store
                    .append(json!({"kind":"actual-source-terminal","value":record}))?;
                self.phase = PeerPhase::Reported;
                self.send(parent, pidfd, record)?;
                Ok(true)
            }
            PeerPhase::Reported => {
                let Some(index) = self.receive(parent, credentials, pidfd, 0)? else {
                    return Ok(false);
                };
                let packet = &parent.packets[index];
                let record: Value = serde_json::from_slice(&packet.bytes)?;
                require(
                    journal::canonical(&record)? == packet.bytes,
                    "cleanup agreement packet not canonical",
                )?;
                let own = holder.creation_terminal_record()?;
                require(
                    record["keeper"] == own,
                    "cleanup agreement replaced original Keeper native history",
                )?;
                let local = holder.creation_source_history()?;
                let remote = self
                    .guardian_ledger
                    .as_mut()
                    .unwrap()
                    .read(&record["guardian"]["source_store"])?;
                let cutoff = record["cutoff"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("cleanup agreement cutoff absent"))?;
                require(
                    cutoff <= self.cutoff.unwrap(),
                    "cleanup agreement extends original Keeper cutoff",
                )?;
                let agreement = if remote.frames.len() == 1 && local.frames.is_empty() {
                    AsymmetricAgreement::issue_no_writes(
                        record["guardian"].clone(),
                        own,
                        remote,
                        local,
                        self.deadline.unwrap(),
                        cutoff,
                    )?
                } else {
                    AsymmetricAgreement::issue(
                        &self.intent,
                        record["guardian"].clone(),
                        own,
                        remote,
                        local,
                        self.deadline.unwrap(),
                        cutoff,
                    )?
                };
                require(
                    agreement.packet() == record,
                    "cleanup agreement selection differs from actual independent histories",
                )?;
                self.cutoff = Some(agreement.native_cutoff);
                self.deadline = Some(self.deadline.unwrap().min(agreement.deadline));
                self.agreement = Some(agreement);
                self.ledger
                    .store
                    .append(json!({"kind":"dual-source-history-agreement","value":record}))?;
                self.phase = PeerPhase::Agreed;
                self.send(parent,pidfd,json!({"schema":"hermit-cleanup-history-ack-v1","nonce":self.intent.nonce,
                    "incarnation":self.intent.incarnation,"agreement":super::hex(&self.agreement.as_ref().unwrap().digest),"cutoff":self.cutoff.unwrap()}))?;
                self.agreement.as_mut().unwrap().peer_acknowledged = true;
                Ok(true)
            }
            PeerPhase::Agreed => {
                let Some(index) = self.receive(parent, credentials, pidfd, 0)? else {
                    return Ok(false);
                };
                let agreement = self.agreement.as_ref().unwrap();
                if agreement.selection.steps.is_empty() {
                    require(
                        parent.packets[index].bytes
                            == journal::canonical(&json!({"schema":"hermit-cleanup-close-v1",
                        "nonce":self.intent.nonce,"incarnation":self.intent.incarnation,
                        "agreement":super::hex(&agreement.digest),"cutoff":self.cutoff.unwrap()}))?,
                        "zero-write peer may only retire aliases, never accept a read/delete grant",
                    )?;
                    self.ledger.complete()?;
                    self.ledger.store.append(json!({"kind":"zero-write-peer-natural-alias-retirement","source_store":agreement.keeper_history.commitment()}))?;
                    self.cursor.phase = CursorPhase::Closed;
                    self.phase = PeerPhase::Closed;
                    return Ok(true);
                }
                let request = json!({"schema":"hermit-cleanup-read-grant-v1","nonce":self.intent.nonce,
                    "incarnation":self.intent.incarnation,"epoch":1,"agreement":super::hex(&agreement.digest),
                    "eligible":agreement.selection.eligible,"cutoff":self.cutoff.unwrap()});
                require(
                    parent.packets[index].bytes == journal::canonical(&request)?
                        && self.cursor.phase == CursorPhase::Source
                        && self.cursor.epoch == 0
                        && agreement.peer_acknowledged,
                    "cleanup read epoch repeated or invalid",
                )?;
                self.cursor.epoch = 1;
                self.cursor.phase = CursorPhase::Peer;
                let controls = holder.creation_controls()?;
                let mut epoch = ReadEpoch {
                    controls,
                    cursor: &self.cursor,
                    epoch: 1,
                    deadline: self.deadline.unwrap(),
                    native_cutoff: self.cutoff.unwrap(),
                    parent: pidfd,
                };
                let definitions = epoch.snapshot(0)?;
                let profile = epoch.snapshot(1)?;
                owner::check_cleanup_observations(
                    &self.intent,
                    &definitions,
                    &profile,
                    agreement.selection.eligible,
                )?;
                self.read_receipts = Some([definitions, profile]);
                let bytes = self.read_receipts.as_ref().unwrap();
                self.ledger.store.append(json!({"kind":"actual-exclusive-cleanup-read","epoch":1,
                    "definitions":{"bytes":bytes[0].len(),"sha256":super::hex(&Sha256::digest(&bytes[0]))},
                    "profile":{"bytes":bytes[1].len(),"sha256":super::hex(&Sha256::digest(&bytes[1]))}}))?;
                self.check(pidfd)?;
                // Irreversible relinquishment precedes send. Failed send never
                // recreates a local read epoch or permits a second scan.
                self.cursor.phase = CursorPhase::Closed;
                self.phase = PeerPhase::Relinquished;
                self.send(parent,pidfd,json!({"schema":"hermit-cleanup-read-done-v1","nonce":self.intent.nonce,
                    "incarnation":self.intent.incarnation,"epoch":1,"agreement":super::hex(&self.agreement.as_ref().unwrap().digest),"cutoff":self.cutoff.unwrap()}))?;
                Ok(true)
            }
            PeerPhase::Relinquished => {
                let Some(index) = self.receive(parent, credentials, pidfd, 0)? else {
                    return Ok(false);
                };
                let bytes = &parent.packets[index].bytes;
                let value: Value = serde_json::from_slice(bytes)?;
                if value["schema"] == "hermit-cleanup-close-v1" {
                    require(
                        *bytes
                            == journal::canonical(
                                &json!({"schema":"hermit-cleanup-close-v1","nonce":self.intent.nonce,
                        "incarnation":self.intent.incarnation,"agreement":super::hex(&self.agreement.as_ref().unwrap().digest),"cutoff":self.cutoff.unwrap()}),
                            )?,
                        "cleanup close identity differs",
                    )?;
                    self.ledger.complete()?;
                    self.ledger.store.append(json!({"kind":"cleanup-peer-alias-retirement-by-natural-exit","value":value}))?;
                    self.phase = PeerPhase::Closed;
                    return Ok(true);
                }
                let o: journal::OwnerSnapshot = serde_json::from_value(value["owner"].clone())?;
                let w: journal::Write = serde_json::from_value(value["write"].clone())?;
                let line = self.intent.command(w.role, 1)?;
                let sequence = self.next_removal;
                require(
                    *bytes
                        == journal::canonical(
                            &json!({"schema":"hermit-cleanup-remove-v1","nonce":self.intent.nonce,
                    "incarnation":self.intent.incarnation,"sequence":sequence,"owner":o,"write":w,"line":super::hex(line.as_bytes()),"cutoff":self.cutoff.unwrap()}),
                        )?,
                    "cleanup peer removal exact sequence/run differs",
                )?;
                self.next_removal += 1;
                self.ledger.append(o, w, line.as_bytes())?;
                self.send(parent,pidfd,json!({"schema":"hermit-cleanup-remove-ack-v1","nonce":self.intent.nonce,
                    "incarnation":self.intent.incarnation,"sequence":sequence,"cutoff":self.cutoff.unwrap()}))?;
                Ok(true)
            }
            PeerPhase::Closed => Ok(false),
        }
    }
    pub(super) fn source_protocol_active(&self) -> bool {
        matches!(self.phase, PeerPhase::Preparing | PeerPhase::Source)
    }
    #[expect(dead_code, reason = "Retained typed creation-recovery protocol is not yet wired into the startup continuation")]
    pub(super) fn complete(&self) -> bool {
        self.phase == PeerPhase::Closed && self.refused.is_none()
    }
}
