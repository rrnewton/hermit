//! Parent-owned failed-source disposal. Original captures stay owned here;
//! neither a Keeper JSON report nor this cleanup emits successful SourceTerminal.
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::time::Duration;
use std::time::Instant;

use serde_json::json;

use super::super::Failure;
use super::super::guardian;
use super::super::owner;
use super::super::require;
use super::ParentLaunchCustody;
use super::Stage;

#[derive(Debug)]
#[must_use = "retain original parent/query custody until joined cleanup"]
#[expect(dead_code, reason = "Legacy failed-parent retirement is not constructed by the maintained entry")]
pub(in super::super) struct ParentFailedRetirement {
    parent: ParentLaunchCustody,
    source_failure: Option<Failure>,
    supplied_origin: Option<u64>,
    deadline: Option<Instant>,
    terminal_query: Option<owner::ManagerQuery>,
    terminal_snapshot: Option<owner::ManagerSnapshot>,
    agreement: Option<guardian::EarlyCustodyAgreement>,
    agreement_packet: Option<Vec<u8>>,
    keeper_acknowledged: bool,
    query_cleanup_failure: Option<Failure>,
    forget: Option<owner::ParentForgetFailed>,
    absence_query: Option<owner::ManagerQuery>,
    absence_snapshot: Option<owner::ManagerSnapshot>,
    cleanup_failure: Option<Failure>,
}
/// Private construction follows actual retained terminal/unlinked/manager/wait
/// checks. The borrowed owners cannot be released or replaced while it exists.
/// This proof is consumed immediately by one failed-unit query start.
#[expect(dead_code, reason = "Legacy failed-parent proof remains private and unissued")]
pub(in super::super) struct ParentFailedUnitProof<'a> {
    parent: &'a ParentLaunchCustody,
    snapshot: &'a owner::ManagerSnapshot,
    deadline: Instant,
    status: i32,
}
#[expect(dead_code, reason = "Legacy failed-parent proof remains private and unissued")]
impl ParentFailedUnitProof<'_> {
    pub(in super::super) fn unit(&self) -> &str {
        &self.parent.unit
    }
    pub(in super::super) fn validate_for_start(&self, deadline: Instant) -> io::Result<()> {
        require(
            deadline <= self.deadline && Instant::now() < deadline,
            "parent failed-unit proof is late or extended",
        )?;
        require(
            self.snapshot.property("InvocationID")
                == self.parent.identity.as_ref().map(|i| i.invocation.as_str())
                && self.snapshot.property("Id") == Some(self.parent.unit.as_str()),
            "parent failed-unit proof identity changed",
        )?;
        // A proved dead pidfd and unlinked retained cgroup cannot become live or
        // linked again. Rechecking those would add a second observation race. The
        // original wrapper remains unreaped and is checked at the effect boundary.
        owner::check_parent_failed_launcher(&self.parent.source_launcher, self.status)
    }
}
#[expect(dead_code, reason = "Preserve checked legacy transitions without claiming maintained-path integration")]
impl ParentFailedRetirement {
    pub fn retain(parent: ParentLaunchCustody) -> Self {
        Self {
            parent,
            source_failure: None,
            supplied_origin: None,
            deadline: None,
            terminal_query: None,
            terminal_snapshot: None,
            agreement: None,
            agreement_packet: None,
            keeper_acknowledged: false,
            query_cleanup_failure: None,
            forget: None,
            absence_query: None,
            absence_snapshot: None,
            cleanup_failure: None,
        }
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            self.cleanup_failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    pub fn begin(&mut self, cause: &io::Error, origin: u64, caller: Instant) -> io::Result<()> {
        if let Some(previous) = self.supplied_origin {
            require(
                previous == origin,
                "parent failed cleanup cannot replace supplied origin",
            )?;
            let prior = self.source_failure.as_ref().unwrap();
            require(
                prior.message == cause.to_string() && prior.errno == cause.raw_os_error(),
                "parent failed cleanup cannot replace source failure",
            )?;
        }
        let result = (|| {
            if self.supplied_origin.is_none() {
                self.supplied_origin = Some(origin);
                self.source_failure = Some(Failure::capture(cause));
            }
            let sampled = Instant::now();
            let now = guardian::monotonic_ns()?;
            require(
                now >= origin && now - origin < 1_000_000_000,
                "parent failed cleanup original source1s expired or future",
            )?;
            let mut earliest = origin;
            if self.parent.refused.is_some() {
                let local = self.parent.failure_origin.ok_or_else(|| {
                    io::Error::other("parent earlier local failure origin unknown")
                })?;
                require(
                    now >= local && now - local < 1_000_000_000,
                    "parent failed cleanup earlier local1s expired or future",
                )?;
                earliest = earliest.min(local);
            }
            let bound = (sampled + Duration::from_nanos(1_000_000_000 - (now - earliest)))
                .min(self.parent.original_deadline)
                .min(caller);
            self.deadline = Some(self.deadline.map_or(bound, |old| old.min(bound)));
            self.require_capture()?;
            self.cutoff().map(|_| ())
        })();
        self.remember(result)
    }
    fn require_capture(&self) -> io::Result<()> {
        require(
            self.parent.stage == Stage::Captured
                && self.parent.pidfd.is_some()
                && self.parent.directory.is_some()
                && self.parent.directory_identity.is_some()
                && self.parent.snapshots.iter().all(Option::is_some),
            "parent failed unit lacks original complete native capture",
        )?;
        let identity = self
            .parent
            .identity
            .as_ref()
            .ok_or_else(|| io::Error::other("parent failed native identity absent"))?;
        for snapshot in self.parent.snapshots.iter().flatten() {
            require(
                self.parent.live_identity(snapshot)? == *identity,
                "parent failed unit original live snapshots disagree",
            )?;
        }
        Ok(())
    }
    pub fn cutoff(&self) -> io::Result<Instant> {
        let origin = self
            .supplied_origin
            .ok_or_else(|| io::Error::other("parent failed cleanup not begun"))?;
        let now = guardian::monotonic_ns()?;
        require(
            now >= origin && now - origin < 1_000_000_000,
            "parent failed cleanup original source1s expired or future",
        )?;
        if self.parent.refused.is_some() {
            let local = self
                .parent
                .failure_origin
                .ok_or_else(|| io::Error::other("parent earlier local failure origin unknown"))?;
            require(
                now >= local && now - local < 1_000_000_000,
                "parent failed cleanup earlier local1s expired or future",
            )?;
        }
        let deadline = self
            .deadline
            .ok_or_else(|| io::Error::other("parent failed cleanup deadline refused"))?;
        require(
            Instant::now() < deadline,
            "parent failed cleanup original deadline expired",
        )?;
        Ok(deadline)
    }
    pub fn check_keeper_record(&self, record: &serde_json::Value) -> io::Result<()> {
        self.require_capture()?;
        self.cutoff()?;
        let native = self.parent.identity.as_ref().unwrap();
        let directory = self.parent.directory_identity.as_ref().unwrap();
        let terminal = self.terminal_snapshot.as_ref().ok_or_else(|| {
            io::Error::other("early agreement precedes actual parent terminal snapshot")
        })?;
        let creator = json!({"unit":self.parent.unit,"nonce":self.parent.nonce,"pid":native.pid,"invocation":native.invocation,
   "credentials":{"pid":native.pid,"uid":unsafe{libc::getuid()},"gid":unsafe{libc::getgid()}},
   "cgroup":native.cgroup,"device":directory.device,"inode":directory.inode,"creator_pidfd_held":true,
   "cgroup_directory_held":true,"cgroup_kill_description_held":false,"kind":"captured-original-cgroup-custody-v1",
   "receiver_uid":unsafe{libc::getuid()},"receiver_euid":unsafe{libc::geteuid()}});
        // Exact complete object equality preserves the existing independent-history
        // manager comparator, including every actual returned property and field.
        require(
            *record
                == json!({"schema":"hermit-failed-source-custody-v1","native_origin":self.supplied_origin.unwrap(),
   "creator":creator,"creator_admitted":false,"controls_held":null,"source_eof":true,"source_retired":true,
   "history_complete_claimed":false,"provider_authority_created":false,
   "original_failure":self.source_failure.as_ref().unwrap().message,"actual_terminal_manager":terminal.evidence(),
   "history":{"next":1,"pending":null,"pairs":[],"failed_pair":null,"create_mask":0}}),
            "keeper first-rejection record differs from exact parent failed identity/history/manager",
        )
    }
    /// A distinct comparator for cleanup-only uncaptured descriptor custody.
    /// The digest correlates the authenticated Keeper's retained query evidence;
    /// it cannot construct a Snapshot or satisfy this parent's native proof.
    pub fn check_partial_keeper_record(&self, record: &serde_json::Value) -> io::Result<()> {
        self.require_capture()?;
        self.cutoff()?;
        require(
            self.terminal_snapshot.is_some(),
            "partial agreement precedes actual parent terminal snapshot",
        )?;
        let native = self.parent.identity.as_ref().unwrap();
        let directory = self.parent.directory_identity.as_ref().unwrap();
        let digest = record["query_custody_sha256"]
            .as_str()
            .ok_or_else(|| io::Error::other("partial query custody digest absent"))?;
        require(
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "partial query custody digest malformed",
        )?;
        let creator = json!({"unit":self.parent.unit,"nonce":self.parent.nonce,"pid":native.pid,"invocation":native.invocation,
   "credentials":{"pid":native.pid,"uid":unsafe{libc::getuid()},"gid":unsafe{libc::getgid()}},
   "cgroup":native.cgroup,"device":directory.device,"inode":directory.inode,"creator_pidfd_held":true,
   "cgroup_directory_held":true,"cgroup_kill_description_held":false,"kind":"uncaptured-original-descriptor-custody-v1",
   "creator_captured":false,"creator_admitted":false,"receiver_uid":unsafe{libc::getuid()},"receiver_euid":unsafe{libc::geteuid()}});
        require(
            *record
                == json!({"schema":"hermit-uncaptured-source-custody-v1","native_origin":self.supplied_origin.unwrap(),
   "creator":creator,"creator_captured":false,"creator_admitted":false,"controls_held":null,"source_eof":true,
   "source_retired":true,"queries_retired":true,"query_custody_sha256":digest,"manager_snapshot_constructed":false,
   "history_complete_claimed":false,"source_terminal_issued":false,"provider_authority_created":false,
   "original_failure":self.source_failure.as_ref().unwrap().message,
   "history":{"next":1,"pending":null,"pairs":[],"failed_pair":null,"create_mask":0}}),
            "uncaptured Keeper record differs from exact parent failed identity/custody/history",
        )
    }
    pub fn identity_record(&self) -> io::Result<serde_json::Value> {
        self.require_capture()?;
        self.cutoff()?;
        let native = self.parent.identity.as_ref().unwrap();
        Ok(
            json!({"unit":self.parent.unit,"nonce":self.parent.nonce,"pid":native.pid,"invocation":native.invocation,"native_origin":self.supplied_origin}),
        )
    }
    pub fn agreement_packet(&mut self) -> io::Result<&[u8]> {
        self.cutoff()?;
        let agreement = self
            .agreement
            .as_ref()
            .ok_or_else(|| io::Error::other("parent early agreement absent"))?;
        agreement.check_parent(self)?;
        if self.agreement_packet.is_none() {
            let packet = super::super::journal::canonical(
                &json!({"schema":agreement.packet_schema(),"agreement":agreement.evidence()}),
            )?;
            require(
                packet.len() <= 4096,
                "early agreement exceeds original packet bound",
            )?;
            self.agreement_packet = Some(packet);
        }
        Ok(self.agreement_packet.as_ref().unwrap())
    }
    pub fn accept_keeper_agreement(
        &mut self,
        packet: &super::super::wire::Packet,
        peer: super::super::wire::Credentials,
    ) -> io::Result<()> {
        require(
            !self.keeper_acknowledged,
            "parent early keeper ACK cannot repeat",
        )?;
        let result = (|| {
            self.cutoff()?;
            packet.exact(0, peer)?;
            let sent = self
                .agreement_packet
                .as_ref()
                .ok_or_else(|| io::Error::other("parent early agreement was not encoded"))?;
            use sha2::Digest;
            let expected = json!({"schema":self.agreement.as_ref().unwrap().ack_schema(),"role":"keeper","parent":self.identity_record()?,
    "agreement_sha256":super::super::hex(&sha2::Sha256::digest(sent))});
            require(
                packet.bytes.len() <= 4096
                    && packet.bytes == super::super::journal::canonical(&expected)?,
                "keeper early durable agreement ACK differs",
            )?;
            self.agreement.as_ref().unwrap().check_parent(self)?;
            self.cutoff()?;
            self.keeper_acknowledged = true;
            Ok(())
        })();
        self.remember(result)
    }
    pub fn retain_agreement(
        &mut self,
        agreement: guardian::EarlyCustodyAgreement,
    ) -> io::Result<()> {
        require(
            self.agreement.is_none(),
            "parent failed agreement cannot be replaced",
        )?;
        self.agreement = Some(agreement); // retain before any fallible interpretation
        if let Some(deadline) = &mut self.deadline {
            *deadline = (*deadline).min(self.agreement.as_ref().unwrap().deadline());
        }
        let result = (|| {
            self.cutoff()?;
            self.agreement.as_ref().unwrap().check_parent(self)
        })();
        self.remember(result)
    }
    fn terminal_proof(&self) -> io::Result<Option<ParentFailedUnitProof<'_>>> {
        self.require_capture()?;
        let deadline = self.cutoff()?;
        let snapshot = self
            .terminal_snapshot
            .as_ref()
            .ok_or_else(|| io::Error::other("parent actual terminal snapshot absent"))?;
        Self::native_terminal_proof(&self.parent, snapshot, deadline)
    }
    fn native_terminal_proof<'a>(
        parent: &'a ParentLaunchCustody,
        snapshot: &'a owner::ManagerSnapshot,
        deadline: Instant,
    ) -> io::Result<Option<ParentFailedUnitProof<'a>>> {
        let native = parent.identity.as_ref().unwrap();
        let pid = native.pid.to_string();
        require(
            snapshot.unit() == parent.unit
                && snapshot.property("Id") == Some(parent.unit.as_str())
                && snapshot.property("LoadState") == Some("loaded")
                && snapshot.property("InvocationID") == Some(native.invocation.as_str())
                && snapshot.property("ExecMainPID") == Some(pid.as_str())
                && snapshot.property("MainPID") == Some("0")
                && snapshot
                    .property("ControlGroup")
                    .is_some_and(|group| group.is_empty() || group == native.cgroup),
            "parent terminal manager lost original unit/creator/invocation",
        )?;
        require(
            snapshot.property("ActiveState") == Some("failed")
                && snapshot.property("SubState") == Some("failed")
                && snapshot.property("Result") == Some("exit-code")
                && snapshot.property("ExecMainCode") == Some("1"),
            "parent failed cleanup cannot adopt natural or non-exit failure",
        )?;
        let text = snapshot
            .property("ExecMainStatus")
            .ok_or_else(|| io::Error::other("parent failed status absent"))?;
        let status: i32 = text.parse().map_err(io::Error::other)?;
        require(
            status > 0 && status <= 255 && status.to_string() == text,
            "parent failed status malformed",
        )?;
        let actual = owner::read_retained_cgroup(
            parent.pidfd.as_ref().unwrap().as_fd(),
            parent.directory.as_ref().unwrap().as_fd(),
            parent.directory_identity.as_ref().unwrap(),
        )?;
        match actual {
            owner::CgroupReadbackProgress::Pending(_) => return Ok(None),
            owner::CgroupReadbackProgress::Observed(actual) => {
                if !actual.creator_terminal || !actual.unlinked {
                    return Ok(None);
                }
            }
        }
        owner::check_parent_failed_launcher(&parent.source_launcher, status)?;
        require(
            Instant::now() < deadline,
            "parent terminal proof exceeded original deadline",
        )?;
        Ok(Some(ParentFailedUnitProof {
            parent,
            snapshot,
            deadline,
            status,
        }))
    }
    pub fn progress_terminal(&mut self) -> io::Result<bool> {
        let result = (|| {
            if let Some(error) = &self.cleanup_failure {
                return Err(error.error());
            }
            self.require_capture()?;
            let deadline = self.cutoff()?;
            if !owner::terminal(self.parent.pidfd.as_ref().unwrap().as_raw_fd())?
                || !owner::parent_launcher_terminal(&self.parent.source_launcher)?
            {
                return Ok(false);
            }
            if self.terminal_query.is_none() {
                self.terminal_query = Some(owner::ManagerQuery::retain(self.parent.unit.clone()));
                self.terminal_query.as_mut().unwrap().start()?;
            }
            if self.terminal_snapshot.is_none() {
                self.terminal_snapshot = self.terminal_query.as_mut().unwrap().poll(deadline)?;
            }
            if self.terminal_snapshot.is_none() {
                return Ok(false);
            }
            Ok(self.terminal_proof()?.is_some())
        })();
        self.remember(result)
    }
    pub fn progress_manager(&mut self) -> io::Result<bool> {
        require(
            self.agreement.is_some() && self.keeper_acknowledged,
            "parent failed unit lacks both durable holder agreements",
        )?;
        let result = (|| {
            if let Some(error) = &self.cleanup_failure {
                return Err(error.error());
            }
            self.agreement
                .as_ref()
                .ok_or_else(|| {
                    io::Error::other("parent failed unit lacks actual durable asymmetric agreement")
                })?
                .check_parent(self)?;
            let deadline = self.cutoff()?;
            if self.forget.is_none() {
                let Some(proof) = self.terminal_proof()? else {
                    return Ok(false);
                };
                self.forget = Some(owner::ParentForgetFailed::retain(&proof));
            }
            if !self.forget.as_ref().unwrap().started() {
                // Borrow independent fields: native parent/snapshot remain immutable while
                // their separate retained query records its one-shot spawn.
                let Some(proof) = Self::native_terminal_proof(
                    &self.parent,
                    self.terminal_snapshot.as_ref().unwrap(),
                    deadline,
                )?
                else {
                    return Ok(false);
                };
                self.forget.as_mut().unwrap().start(proof, deadline)?;
                self.cutoff()?;
            }
            if !self.forget.as_mut().unwrap().poll(deadline)? {
                return Ok(false);
            }
            if self.absence_query.is_none() {
                self.absence_query = Some(owner::ManagerQuery::retain(self.parent.unit.clone()));
                self.absence_query.as_mut().unwrap().start()?;
                self.cutoff()?;
            }
            if self.absence_snapshot.is_none() {
                self.absence_snapshot = self.absence_query.as_mut().unwrap().poll(deadline)?;
            }
            let Some(absent) = &self.absence_snapshot else {
                return Ok(false);
            };
            require(
                absent.unit() == self.parent.unit
                    && absent.property("Id") == Some(self.parent.unit.as_str())
                    && absent.property("LoadState") == Some("not-found")
                    && absent.property("InvocationID") == Some("")
                    && absent.property("MainPID") == Some("0")
                    && absent.property("ControlGroup") == Some(""),
                "parent failed unit not actually absent after original reset",
            )?;
            self.cutoff()?;
            Ok(true)
        })();
        self.remember(result)
    }
    /// Drive every retained query after a local failure without parsing cleanup
    /// bytes as a Snapshot, repeating a native effect, or refreshing any origin.
    pub fn retire_query_custody(&mut self) -> io::Result<bool> {
        let deadline = self.cutoff()?;
        let cause = self
            .cleanup_failure
            .as_ref()
            .or(self.source_failure.as_ref())
            .ok_or_else(|| io::Error::other("parent query cleanup precedes failure"))?
            .error();
        let initial = if self.parent.snapshots[0].is_some() {
            Ok(owner::QueryRetirement::Retired)
        } else {
            self.parent.initial.retire_custody(deadline, &cause)
        };
        let recheck = if self.parent.snapshots[1].is_some() {
            Ok(owner::QueryRetirement::Retired)
        } else {
            self.parent.recheck.retire_custody(deadline, &cause)
        };
        let terminal = if self.terminal_snapshot.is_some() {
            Ok(owner::QueryRetirement::Retired)
        } else {
            self.terminal_query
                .as_mut()
                .map_or(Ok(owner::QueryRetirement::NoChild), |q| {
                    q.retire_custody(deadline, &cause)
                })
        };
        let forget = self
            .forget
            .as_mut()
            .map_or(Ok(owner::QueryRetirement::NoChild), |q| {
                q.retire_custody(deadline, &cause)
            });
        let absence = if self.absence_snapshot.is_some() {
            Ok(owner::QueryRetirement::Retired)
        } else {
            self.absence_query
                .as_mut()
                .map_or(Ok(owner::QueryRetirement::NoChild), |q| {
                    q.retire_custody(deadline, &cause)
                })
        };
        let mut complete = true;
        for result in [initial, recheck, terminal, forget, absence] {
            match result {
                Ok(owner::QueryRetirement::Pending) => complete = false,
                Ok(owner::QueryRetirement::NoChild | owner::QueryRetirement::Retired) => {}
                Err(error) => {
                    complete = false;
                    self.query_cleanup_failure
                        .get_or_insert_with(|| Failure::capture(&error));
                }
            }
        }
        self.cutoff()?;
        if let Some(error) = &self.query_cleanup_failure {
            return Err(error.error());
        }
        Ok(complete)
    }
    pub fn diagnostics(&self) -> serde_json::Value {
        json!({"scope":"actual failed parent custody only; no Creator/SourceTerminal/Provider",
  "parent":self.parent.diagnostics(),"source_failure":self.source_failure.as_ref().map(|e|&e.message),"supplied_origin":self.supplied_origin,
  "cleanup_failure":self.cleanup_failure.as_ref().map(|e|&e.message),"terminal_query":self.terminal_query.as_ref().map(owner::ManagerQuery::evidence),
  "terminal_snapshot":self.terminal_snapshot.as_ref().map(owner::ManagerSnapshot::evidence),"agreement_retained":self.agreement.is_some(),"keeper_durable_agreement_acknowledged":self.keeper_acknowledged,
  "query_cleanup_failure":self.query_cleanup_failure.as_ref().map(|e|&e.message),
  "forget":self.forget.as_ref().map(owner::ParentForgetFailed::evidence),"absence_query":self.absence_query.as_ref().map(owner::ManagerQuery::evidence),
  "absence_snapshot":self.absence_snapshot.as_ref().map(owner::ManagerSnapshot::evidence),"source_terminal_issued":false,"provider_authority_issued":false})
    }
}
