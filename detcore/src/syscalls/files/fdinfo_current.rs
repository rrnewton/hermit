/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Separately admitted represented-current ordinary-object observations.
//! The owned facts below never manufacture physical proc/task authority.

use reverie::CurrentFdinfoObjectReceipt;
use reverie::CurrentFdinfoProofGap;
use reverie::CurrentFdinfoRead;
use reverie::CurrentFdinfoSnapshotFacts;
use reverie::CurrentFdinfoSource;
use reverie::CurrentFdinfoSourceBinding;
use reverie::Error;
use reverie::Guest;
use reverie::SignalTaskIdentity;
use reverie::syscalls::Errno;
use serde::Deserialize;
use serde::Serialize;

use super::*;
use crate::fdinfo_projection::FdinfoIncarnation;

const CURRENT_FACT_KIND: u8 = 6;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct CurrentScope {
    incarnation: FdinfoIncarnation,
    // Canonical projection ownership is distinct from FileMetadata's actual
    // table after the existing authenticated exec-reentry transaction.
    files: FilesId,
    actual_files: FilesId,
    source_fd: RawFd,
    source: OpenFileId,
    target_fd: RawFd,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct SourceBinding {
    reader: SignalTaskIdentity,
    subject: SignalTaskIdentity,
    target_fd: RawFd,
}

impl From<CurrentFdinfoSourceBinding> for SourceBinding {
    fn from(binding: CurrentFdinfoSourceBinding) -> Self {
        Self {
            reader: binding.reader,
            subject: binding.subject,
            target_fd: binding.target_fd,
        }
    }
}

impl SourceBinding {
    fn selects_current_process(self, scope: &CurrentScope) -> bool {
        self.reader.process == self.subject.process && self.target_fd == scope.target_fd
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum SourceChoice {
    Unsupported,
    HoldUnavailable(ProcIdentityGap),
    Selected(SourceBinding),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum ProofGap {
    SourceChanged,
    TargetChanged,
    ActorChanged,
    DifferentTable,
    RetiredSubject,
    UnsupportedObject,
    ObjectObservationUnavailable,
    UnsupportedMountView,
    CleanupUnavailable,
    IncompleteOrStaleSnapshot,
}

impl From<CurrentFdinfoProofGap> for ProofGap {
    fn from(gap: CurrentFdinfoProofGap) -> Self {
        match gap {
            CurrentFdinfoProofGap::SourceChanged => Self::SourceChanged,
            CurrentFdinfoProofGap::TargetChanged => Self::TargetChanged,
            CurrentFdinfoProofGap::ActorChanged => Self::ActorChanged,
            CurrentFdinfoProofGap::DifferentTable => Self::DifferentTable,
            CurrentFdinfoProofGap::RetiredSubject => Self::RetiredSubject,
            CurrentFdinfoProofGap::UnsupportedObject => Self::UnsupportedObject,
            CurrentFdinfoProofGap::ObjectObservationUnavailable => {
                Self::ObjectObservationUnavailable
            }
            CurrentFdinfoProofGap::UnsupportedMountView => Self::UnsupportedMountView,
            CurrentFdinfoProofGap::CleanupUnavailable => Self::CleanupUnavailable,
            CurrentFdinfoProofGap::IncompleteOrStaleSnapshot => Self::IncompleteOrStaleSnapshot,
        }
    }
}

impl ProofGap {
    fn source_unjoined(self) -> bool {
        matches!(
            self,
            Self::SourceChanged
                | Self::ActorChanged
                | Self::DifferentTable
                | Self::RetiredSubject
                | Self::IncompleteOrStaleSnapshot
        )
    }

    fn projection_gap(self) -> FdinfoProjectionGap {
        match self {
            Self::RetiredSubject | Self::ActorChanged => FdinfoProjectionGap::RetiredTask,
            Self::SourceChanged | Self::TargetChanged | Self::DifferentTable => {
                FdinfoProjectionGap::RejectedUpdate
            }
            Self::UnsupportedMountView => FdinfoProjectionGap::UnsupportedMountView,
            Self::ObjectObservationUnavailable
            | Self::CleanupUnavailable
            | Self::UnsupportedObject
            | Self::IncompleteOrStaleSnapshot => FdinfoProjectionGap::MissingObject,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct SnapshotFacts {
    origin: u64,
    fresh_render: bool,
    complete: bool,
    reached_eof: bool,
    sequence_restored: bool,
    source_cleanup: Result<(), i32>,
    target_cleanup: Option<Result<(), i32>>,
    read_cleanup: Option<Result<(), i32>>,
}

impl From<CurrentFdinfoSnapshotFacts> for SnapshotFacts {
    fn from(facts: CurrentFdinfoSnapshotFacts) -> Self {
        let errno = |result: Result<(), Errno>| result.map_err(|error| error as i32);
        Self {
            origin: facts.origin,
            fresh_render: facts.fresh_render,
            complete: facts.complete,
            reached_eof: facts.reached_eof,
            sequence_restored: facts.sequence_restored,
            source_cleanup: errno(facts.source_cleanup),
            target_cleanup: facts.target_cleanup.map(errno),
            read_cleanup: facts.read_cleanup.map(errno),
        }
    }
}

impl SnapshotFacts {
    fn cleanup_well_formed(self) -> bool {
        let valid = |result: Result<(), i32>| match result {
            Ok(()) => true,
            Err(errno) => (1..=4095).contains(&errno),
        };
        valid(self.source_cleanup)
            && self.target_cleanup.is_none_or(valid)
            && self.read_cleanup.is_none_or(valid)
    }

    fn is_complete(self) -> bool {
        self.origin == 0
            && self.fresh_render
            && self.complete
            && self.reached_eof
            && self.sequence_restored
    }

    fn cleanup_succeeded(self) -> bool {
        self.source_cleanup.is_ok()
            && self.target_cleanup.is_none_or(|result| result.is_ok())
            && self.read_cleanup.is_none_or(|result| result.is_ok())
    }

    fn positive_cleanup(self) -> bool {
        self.cleanup_succeeded()
            && self.target_cleanup == Some(Ok(()))
            && self.read_cleanup == Some(Ok(()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ObjectReceipt {
    binding: SourceBinding,
    object: FdinfoRawObject,
    status_flags: i32,
    target_cloexec: bool,
    mount_identity_order: Vec<u64>,
}

impl From<CurrentFdinfoObjectReceipt> for ObjectReceipt {
    fn from(receipt: CurrentFdinfoObjectReceipt) -> Self {
        Self {
            binding: receipt.binding.into(),
            object: FdinfoRawObject {
                inode: RawInode::new(
                    libc::makedev(receipt.object.device_major, receipt.object.device_minor),
                    receipt.object.inode,
                ),
                mode: receipt.object.file_type,
                mount_id: receipt.object.mount_id,
            },
            status_flags: receipt.status_flags,
            target_cloexec: receipt.target_cloexec,
            mount_identity_order: receipt.mount_identity_order,
        }
    }
}

impl ObjectReceipt {
    fn matches_render(&self, binding: SourceBinding, bytes: &[u8]) -> bool {
        let flags = self.status_flags
            | if self.target_cloexec {
                libc::O_CLOEXEC
            } else {
                0
            };
        self.binding == binding
            && crate::procfs::parse_fdinfo_fields(bytes).is_some_and(|fields| {
                fields.inode == self.object.inode.ino
                    && fields.mount_id == self.object.mount_id
                    && fields.flags == flags
            })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum Observation {
    Discarded {
        reason: FdinfoProjectionGap,
        facts: SnapshotFacts,
    },
    Snapshot {
        bytes: Vec<u8>,
        receipt: Result<ObjectReceipt, ProofGap>,
        facts: SnapshotFacts,
    },
    SnapshotUnavailable {
        gap: ProofGap,
        facts: SnapshotFacts,
    },
    ReadErrno {
        errno: i32,
        facts: SnapshotFacts,
    },
}

impl Observation {
    fn facts(&self) -> SnapshotFacts {
        match self {
            Self::Discarded { facts, .. }
            | Self::Snapshot { facts, .. }
            | Self::SnapshotUnavailable { facts, .. }
            | Self::ReadErrno { facts, .. } => *facts,
        }
    }
}

impl From<CurrentFdinfoRead> for Observation {
    fn from(read: CurrentFdinfoRead) -> Self {
        match read {
            CurrentFdinfoRead::Success {
                bytes,
                receipt,
                facts,
            } => Self::Snapshot {
                bytes,
                receipt: receipt.map(Into::into).map_err(Into::into),
                facts: facts.into(),
            },
            CurrentFdinfoRead::SnapshotUnavailable { gap, facts } => Self::SnapshotUnavailable {
                gap: gap.into(),
                facts: facts.into(),
            },
            CurrentFdinfoRead::ReadErrno { errno, facts } => Self::ReadErrno {
                errno: errno as i32,
                facts: facts.into(),
            },
        }
    }
}

/// Kind and phase are both checked by the existing exact typed decoder. No
/// backend lease, host descriptor or Arc address is a serialized field.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum CurrentFact {
    SourceChoice {
        scope: CurrentScope,
        choice: SourceChoice,
    },
    Observation {
        scope: CurrentScope,
        observation: Observation,
    },
}

impl CurrentFact {
    fn source_choice(self, expected: &CurrentScope) -> Result<SourceChoice, Error> {
        match self {
            Self::SourceChoice { scope, choice } if scope == *expected => {
                // Replay must enforce the same semantic error whitelist as
                // live construction. A typed fatal gap is not raw fallback.
                if let SourceChoice::HoldUnavailable(gap) = &choice
                    && !gap.cleanup_preserves_completed_read()
                {
                    return Err(fatal_private(gap.clone().into_error()));
                }
                Ok(choice)
            }
            _ => Err(integrity(
                "expected matching current fdinfo SourceChoice phase",
            )),
        }
    }

    fn observation(self, expected: &CurrentScope) -> Result<Observation, Error> {
        match self {
            Self::Observation { scope, observation } if scope == *expected => Ok(observation),
            _ => Err(integrity(
                "expected matching current fdinfo Observation phase",
            )),
        }
    }
}

pub(super) enum CurrentSnapshot {
    Unsupported,
    NoInstall,
    Captured {
        contents: Vec<u8>,
        projection: FdinfoProjectionOutcome,
    },
}

fn integrity(message: &'static str) -> Error {
    Error::Tool(anyhow::anyhow!(message))
}

/// These errors arose in private ownership or fact plumbing, not the retained
/// description's raw read. Only Observation::ReadErrno can supply that errno.
fn fatal_private(error: Error) -> Error {
    Error::Tool(anyhow::anyhow!(
        "current fdinfo private integrity failure: {error}"
    ))
}

async fn cancel<G, T>(guest: &mut G, key: Option<u64>)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let Some(capture) = key {
        let _ = fdinfo_control(guest, FdinfoControlOperation::CancelCapture { capture }).await;
    }
}

/// Even a failed fact hook explicitly settles the live probe. Its failure is
/// terminal; a private cleanup errno never becomes the guest read's errno.
fn settle_failed_choice(source: Option<Box<dyn CurrentFdinfoSource>>) {
    if let Some(source) = source {
        let result = source.discard();
        tracing::trace!(target: "fdinfo_diagnostic", ?result,
            "current fdinfo source settled after fatal choice hook failure");
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    pub(super) async fn prepare_current_fdinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
        source: &CapturedDetFd,
    ) -> Result<CurrentSnapshot, Error> {
        let Some(target) = source.descriptor().procfs_fdinfo_target() else {
            return Ok(CurrentSnapshot::Unsupported);
        };
        if !guest.config().sequentialize_threads {
            return Ok(CurrentSnapshot::Unsupported);
        }
        let scope = CurrentScope {
            incarnation: FdinfoIncarnation {
                tid: guest.thread_state().dettid,
                mm: guest.thread_state().mm_id,
            },
            files: guest.thread_state().fdinfo_files_id(),
            actual_files: source.files_id(),
            source_fd: call.fd(),
            source: source.descriptor().fdinfo_open_file_id(),
            target_fd: target.fd,
        };
        let replay: Option<CurrentFact> = self
            .fdinfo_replay_capture_fact(guest, CURRENT_FACT_KIND)
            .await
            .map_err(fatal_private)?;
        let mut lease = None;
        let is_replay = replay.is_some();
        let selected = if let Some(recorded) = replay {
            recorded
        } else {
            let choice = match guest.hold_current_fdinfo_source(call.fd()) {
                Ok(Some(source)) => {
                    let binding = (*source.binding()).into();
                    lease = Some(source);
                    SourceChoice::Selected(binding)
                }
                Ok(None) => SourceChoice::Unsupported,
                Err(error) => {
                    let gap = ProcIdentityGap::from_error(error);
                    if !gap.cleanup_preserves_completed_read() {
                        return Err(fatal_private(gap.into_error()));
                    }
                    SourceChoice::HoldUnavailable(gap)
                }
            };
            let observed = CurrentFact::SourceChoice {
                scope: scope.clone(),
                choice,
            };
            match self
                .fdinfo_capture_fact(guest, CURRENT_FACT_KIND, &observed)
                .await
            {
                Ok(recorded) if recorded == observed => recorded,
                Ok(_) => {
                    settle_failed_choice(lease.take());
                    return Err(integrity(
                        "current fdinfo source-choice hook changed a live fact",
                    ));
                }
                Err(error) => {
                    settle_failed_choice(lease.take());
                    return Err(fatal_private(error));
                }
            }
        };
        let choice = match selected.source_choice(&scope) {
            Ok(choice) => choice,
            Err(error) => {
                settle_failed_choice(lease.take());
                return Err(error);
            }
        };
        let binding = match choice {
            SourceChoice::Unsupported => return Ok(CurrentSnapshot::Unsupported),
            SourceChoice::HoldUnavailable(_) => return Ok(CurrentSnapshot::NoInstall),
            SourceChoice::Selected(binding) => binding,
        };
        let actor = guest.signal_task_identity();
        let source_joined = source.files_id() == scope.actual_files
            && guest.thread_state().fdinfo_files_id() == scope.files
            && guest
                .thread_state()
                .procfs_source_still_named(call.fd(), source);
        let control = if !source_joined
            || !binding.selects_current_process(&scope)
            || (!is_replay && actor != Some(binding.reader))
        {
            Err(FdinfoProjectionGap::RejectedUpdate)
        } else if let Some(actor) = actor {
            fdinfo_control(
                guest,
                FdinfoControlOperation::BeginCurrent {
                    files: scope.files,
                    source_fd: scope.source_fd,
                    source: scope.source,
                    target_fd: scope.target_fd,
                    actor,
                },
            )
            .await
            .and_then(|reply| match reply {
                FdinfoControlValue::Capture(key) => Ok(key),
                _ => Err(FdinfoProjectionGap::RejectedUpdate),
            })
        } else {
            Err(FdinfoProjectionGap::RejectedUpdate)
        };
        let key = control.as_ref().ok().copied();
        let observation = if is_replay {
            match self
                .fdinfo_replay_capture_fact::<_, CurrentFact>(guest, CURRENT_FACT_KIND)
                .await
            {
                Ok(Some(recorded)) => match recorded.observation(&scope) {
                    Ok(observation) => observation,
                    Err(error) => {
                        cancel(guest, key).await;
                        return Err(error);
                    }
                },
                Ok(_) => {
                    cancel(guest, key).await;
                    return Err(integrity(
                        "expected matching current fdinfo Observation phase",
                    ));
                }
                Err(error) => {
                    cancel(guest, key).await;
                    return Err(fatal_private(error));
                }
            }
        } else {
            let owned = lease
                .take()
                .expect("selected live fdinfo probe has no source lease");
            let observed = match &control {
                Ok(_) => owned.capture().map(Observation::from),
                Err(reason) => owned.discard().map(|facts| Observation::Discarded {
                    reason: reason.clone(),
                    facts: facts.into(),
                }),
            };
            let observed = match observed {
                Ok(observation) => CurrentFact::Observation {
                    scope: scope.clone(),
                    observation,
                },
                Err(error) => {
                    cancel(guest, key).await;
                    return Err(fatal_private(error));
                }
            };
            match self
                .fdinfo_capture_fact(guest, CURRENT_FACT_KIND, &observed)
                .await
            {
                Ok(recorded) if recorded == observed => match recorded {
                    CurrentFact::Observation { observation, .. } => observation,
                    _ => unreachable!("observation phase constructed above"),
                },
                Ok(_) => {
                    cancel(guest, key).await;
                    return Err(integrity(
                        "current fdinfo observation hook changed a live fact",
                    ));
                }
                Err(error) => {
                    cancel(guest, key).await;
                    return Err(fatal_private(error));
                }
            }
        };
        if !observation.facts().sequence_restored || !observation.facts().cleanup_well_formed() {
            cancel(guest, key).await;
            return Err(integrity(
                "current fdinfo sequence/cleanup facts are incomplete or malformed",
            ));
        }
        match observation {
            Observation::Discarded { facts, .. } => {
                cancel(guest, key).await;
                if facts.fresh_render
                    || facts.complete
                    || facts.reached_eof
                    || facts.target_cleanup.is_some()
                    || facts.read_cleanup.is_some()
                {
                    return Err(integrity(
                        "declined current fdinfo probe unexpectedly captured a record",
                    ));
                }
                Ok(CurrentSnapshot::NoInstall)
            }
            Observation::SnapshotUnavailable { .. } => {
                cancel(guest, key).await;
                Ok(CurrentSnapshot::NoInstall)
            }
            Observation::ReadErrno { errno, .. } => {
                cancel(guest, key).await;
                if !(1..=4095).contains(&errno) {
                    return Err(integrity(
                        "current fdinfo read fact contains an invalid errno",
                    ));
                }
                Err(Errno::new(errno).into())
            }
            Observation::Snapshot {
                bytes,
                receipt,
                facts,
            } => {
                if !facts.is_complete()
                    || match &receipt {
                        Ok(receipt) => receipt.binding != binding,
                        Err(gap) => gap.source_unjoined(),
                    }
                    || !guest
                        .thread_state()
                        .procfs_source_still_named(call.fd(), source)
                    || guest.thread_state().fdinfo_files_id() != scope.files
                {
                    cancel(guest, key).await;
                    return Ok(CurrentSnapshot::NoInstall);
                }
                let receipt = if !facts.positive_cleanup() {
                    Err(FdinfoProjectionGap::MissingObject)
                } else {
                    receipt
                        .map_err(ProofGap::projection_gap)
                        .and_then(|receipt| {
                            if receipt.matches_render(binding, &bytes) {
                                Ok(receipt)
                            } else {
                                Err(FdinfoProjectionGap::ObjectChanged)
                            }
                        })
                };
                let projection = match (control, actor, receipt) {
                    (Ok(capture), Some(actor), Ok(receipt)) => {
                        self.project_current_fdinfo(guest, capture, actor, &scope, receipt)
                            .await?
                    }
                    (control, _, receipt) => {
                        cancel(guest, key).await;
                        FdinfoProjectionOutcome::Unavailable {
                            reason: control
                                .err()
                                .or_else(|| receipt.err())
                                .unwrap_or(FdinfoProjectionGap::RejectedUpdate),
                            acquisition: None,
                        }
                    }
                };
                Ok(CurrentSnapshot::Captured {
                    contents: bytes,
                    projection,
                })
            }
        }
    }

    async fn project_current_fdinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        capture: u64,
        actor: SignalTaskIdentity,
        scope: &CurrentScope,
        receipt: ObjectReceipt,
    ) -> Result<FdinfoProjectionOutcome, Error> {
        let unavailable = |reason| FdinfoProjectionOutcome::Unavailable {
            reason,
            acquisition: None,
        };
        let authority = match fdinfo_control(
            guest,
            FdinfoControlOperation::FinishCurrent {
                capture,
                object: receipt.object,
                actor,
            },
        )
        .await
        {
            Ok(FdinfoControlValue::CapturedCurrent(value)) => value,
            Ok(_) => {
                cancel(guest, Some(capture)).await;
                return Ok(unavailable(FdinfoProjectionGap::RejectedUpdate));
            }
            Err(reason) => {
                cancel(guest, Some(capture)).await;
                return Ok(unavailable(reason));
            }
        };
        let target = &authority.target;
        if target.incarnation != scope.incarnation
            || target.files != scope.files
            || target.source_fd != scope.source_fd
            || target.source != scope.source
            || target.target_fd != scope.target_fd
            || target.slot.cloexec != receipt.target_cloexec
        {
            return Ok(unavailable(FdinfoProjectionGap::RejectedUpdate));
        }
        // All backend owners and the non-wire capture token have settled.
        // Use the same ordinary accounted pools as fstat/fdinfo, including
        // actual ResourceID policy for replacement files at descriptors 0–2.
        let inode =
            match deterministic_stdio_inode_for_resource(scope.target_fd, target.ofd.resource) {
                Some(inode) => inode,
                None => {
                    determinize_inode_observing_mtime(
                        guest,
                        receipt.object.inode,
                        None,
                        ObservedMtime::Unobserved,
                        InodeSighting::Descriptor,
                    )
                    .await
                    .0
                }
            };
        let mount_id = if receipt.object.mount_id == 0 {
            0
        } else {
            let Some(id) = determinize_mount_id(
                guest,
                receipt.object.mount_id,
                Some(receipt.mount_identity_order),
            )
            .await
            else {
                return Ok(unavailable(FdinfoProjectionGap::UnsupportedMountView));
            };
            id
        };
        Ok(FdinfoProjectionOutcome::CurrentActor {
            identity: FdinfoIdentity {
                inode: inode.as_raw(),
                flags: target.fdinfo_flags(),
                open_file: Some(authority.logical_open_file.deterministic_socket_cookie()),
                mount_id,
            },
            target: authority.target,
            logical_open_file: authority.logical_open_file,
        })
    }
}

#[cfg(test)]
mod tests {
    use reverie::Pid;
    use reverie::SignalProcessId;

    use super::*;

    #[test]
    fn current_fdinfo_facts_reject_foreign_generations_suffixes_and_wrong_replay_phases() {
        let tid = DetTid::from_raw(3);
        let scope = CurrentScope {
            incarnation: FdinfoIncarnation {
                tid,
                mm: MmId::initial(tid),
            },
            files: FilesId::initial(tid),
            actual_files: FilesId::initial(tid),
            source_fd: 5,
            source: OpenFileId::new(tid, 1),
            target_fd: 0,
        };
        let reader = SignalTaskIdentity {
            process: SignalProcessId {
                tgid: Pid::from_raw(3),
                generation: 7,
            },
            tid: Pid::from_raw(3),
            task_generation: 9,
        };
        let binding = SourceBinding {
            reader,
            subject: reader,
            target_fd: 0,
        };
        assert!(binding.selects_current_process(&scope));
        let mut reused = binding;
        reused.subject.process.generation += 1;
        assert!(!reused.selects_current_process(&scope));
        let mut different_target = binding;
        different_target.target_fd = 1;
        assert!(!different_target.selects_current_process(&scope));

        let facts = SnapshotFacts {
            origin: 0,
            fresh_render: true,
            complete: true,
            reached_eof: true,
            sequence_restored: true,
            source_cleanup: Ok(()),
            target_cleanup: Some(Ok(())),
            read_cleanup: Some(Ok(())),
        };
        assert!(facts.is_complete() && facts.positive_cleanup());
        assert!(!SnapshotFacts { origin: 1, ..facts }.is_complete());
        assert!(
            !SnapshotFacts {
                fresh_render: false,
                ..facts
            }
            .is_complete()
        );
        assert!(
            !SnapshotFacts {
                reached_eof: false,
                ..facts
            }
            .is_complete()
        );
        assert!(
            !SnapshotFacts {
                sequence_restored: false,
                ..facts
            }
            .is_complete()
        );
        let failed_cleanup = SnapshotFacts {
            read_cleanup: Some(Err(libc::EIO)),
            ..facts
        };
        assert!(failed_cleanup.is_complete());
        assert!(!failed_cleanup.positive_cleanup());
        assert!(ProofGap::SourceChanged.source_unjoined());
        assert!(ProofGap::DifferentTable.source_unjoined());
        assert!(!ProofGap::TargetChanged.source_unjoined());
        assert!(
            !SnapshotFacts {
                source_cleanup: Err(0),
                ..facts
            }
            .cleanup_well_formed()
        );
        assert!(matches!(fatal_private(Errno::EIO.into()), Error::Tool(_)));
        assert!(matches!(
            fatal_private(Error::Io(std::io::Error::from_raw_os_error(libc::EIO))),
            Error::Tool(_)
        ));

        for gap in [
            ProcIdentityGap::Errno(libc::EIO),
            ProcIdentityGap::Io {
                errno: Some(libc::EIO),
            },
        ] {
            let unavailable = CurrentFact::SourceChoice {
                scope: scope.clone(),
                choice: SourceChoice::HoldUnavailable(gap),
            };
            assert!(matches!(
                unavailable.source_choice(&scope),
                Ok(SourceChoice::HoldUnavailable(_))
            ));
        }
        for gap in [
            ProcIdentityGap::Tool("hook failed".into()),
            ProcIdentityGap::Io { errno: None },
            ProcIdentityGap::NotRequested,
            ProcIdentityGap::CaptureFact {
                result: Box::new(Err(ProcIdentityGap::Errno(libc::EIO))),
                primary: Box::new(ProcIdentityGap::Errno(libc::EIO)),
            },
        ] {
            let unavailable = CurrentFact::SourceChoice {
                scope: scope.clone(),
                choice: SourceChoice::HoldUnavailable(gap),
            };
            assert!(matches!(
                unavailable.source_choice(&scope),
                Err(Error::Tool(_))
            ));
        }

        let choice = CurrentFact::SourceChoice {
            scope: scope.clone(),
            choice: SourceChoice::Selected(binding),
        };
        assert!(choice.clone().source_choice(&scope).is_ok());
        assert!(choice.observation(&scope).is_err());
        let discarded = CurrentFact::Observation {
            scope: scope.clone(),
            observation: Observation::Discarded {
                reason: FdinfoProjectionGap::RejectedUpdate,
                facts: SnapshotFacts {
                    fresh_render: false,
                    complete: false,
                    reached_eof: false,
                    target_cleanup: None,
                    read_cleanup: None,
                    ..facts
                },
            },
        };
        assert!(discarded.clone().source_choice(&scope).is_err());
        let mut different_source = scope.clone();
        different_source.source = OpenFileId::new(tid, 2);
        assert!(discarded.clone().observation(&different_source).is_err());
        let mut different_table = scope.clone();
        different_table.actual_files = FilesId::forked(DetTid::from_raw(4));
        assert!(discarded.observation(&different_table).is_err());
    }
}
