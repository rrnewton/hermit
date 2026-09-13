// Copyright (c) Meta Platforms, Inc. and affiliates.
// Licensed under the BSD-style license in the LICENSE file.

//! Target-owned delivery of a scheduler-ordered normal child exit.
//!
//! A delivery is a control exchange, not a resource grant. The operation retains its
//! original request across the exchange; cancellation retains an in-flight attempt
//! until its callback acknowledges success or failure.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

use crate::ivar::Ivar;
use crate::resources::Resources;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::MmId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NormalExit {
    pub status: u8,
    pub uid: u32,
    pub user_ticks: u64,
    pub system_ticks: u64,
}

impl NormalExit {
    pub(crate) fn capture<T>(state: &mut crate::tool_local::ThreadState<T>, status: i32) -> Self {
        let cpu = state.process_cpu_time();
        Self {
            status: status as u8,
            // Credential queries already expose the configured virtual root identity.
            uid: 0,
            user_ticks: crate::syscalls::clock_ticks(cpu.user),
            system_ticks: crate::syscalls::clock_ticks(cpu.system),
        }
    }

    fn siginfo(self, child: DetPid) -> [u8; 128] {
        let mut bytes = [0; 128];
        bytes[0..4].copy_from_slice(&libc::SIGCHLD.to_ne_bytes());
        bytes[8..12].copy_from_slice(&libc::CLD_EXITED.to_ne_bytes());
        bytes[16..20].copy_from_slice(&child.as_raw().to_ne_bytes());
        bytes[20..24].copy_from_slice(&self.uid.to_ne_bytes());
        bytes[24..28].copy_from_slice(&i32::from(self.status).to_ne_bytes());
        bytes[32..40].copy_from_slice(&self.user_ticks.to_ne_bytes());
        bytes[40..48].copy_from_slice(&self.system_ticks.to_ne_bytes());
        bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Identity of the original resource operation, retained across a child-exit control.
pub struct OperationId {
    pub(crate) tid: DetTid,
    pub(crate) mm: MmId,
    pub(crate) sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Delivery {
    pub id: u64,
    pub child: DetPid,
    pub child_mm: MmId,
    pub parent: DetPid,
    pub exit: NormalExit,
    pub deadline: LogicalTime,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A complete child-exit event addressed to one stopped resource callback.
pub struct Command {
    pub(crate) operation: OperationId,
    pub(crate) delivery: Delivery,
    pub(crate) signal: i32,
    #[serde(with = "siginfo_wire")]
    pub(crate) siginfo: [u8; 128],
}

impl Command {
    pub(crate) fn new(operation: OperationId, delivery: Delivery) -> Self {
        Self {
            operation,
            signal: libc::SIGCHLD,
            siginfo: delivery.exit.siginfo(delivery.child),
            delivery,
        }
    }

    pub fn event(
        &self,
        tid: DetTid,
        mm: MmId,
        parent: DetPid,
        backend_pid: reverie::Pid,
    ) -> Result<reverie::SignalEvent, reverie::Errno> {
        if self.operation.tid != tid
            || self.operation.mm != mm
            || self.delivery.parent != parent
            || parent.as_raw() <= 0
            || self.delivery.child.as_raw() <= 0
            || backend_pid.as_raw() <= 0
            || self.signal != libc::SIGCHLD
            || self.siginfo != self.delivery.exit.siginfo(self.delivery.child)
            || self.delivery.exit.user_ticks > i64::MAX as u64
            || self.delivery.exit.system_ticks > i64::MAX as u64
        {
            return Err(reverie::Errno::EINVAL);
        }
        // Backend identity is supplied by this Guest; the guest-visible child PID stays
        // in siginfo. A virtual identifier never reaches a host signal syscall here.
        reverie::SignalEvent::new(
            self.signal,
            self.siginfo,
            reverie::SignalTarget::Process { pid: backend_pid },
        )
    }
}

mod siginfo_wire {
    use serde::Deserializer;
    use serde::Serializer;
    use serde::de::Error;
    use serde::de::SeqAccess;
    use serde::de::Visitor;
    use serde::ser::SerializeTuple;

    pub fn serialize<S: Serializer>(value: &[u8; 128], serializer: S) -> Result<S::Ok, S::Error> {
        let mut tuple = serializer.serialize_tuple(128)?;
        for byte in value {
            tuple.serialize_element(byte)?;
        }
        tuple.end()
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 128], D::Error> {
        struct Info;
        impl<'de> Visitor<'de> for Info {
            type Value = [u8; 128];
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("exactly 128 siginfo bytes")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut bytes = [0; 128];
                for (index, byte) in bytes.iter_mut().enumerate() {
                    *byte = seq
                        .next_element()?
                        .ok_or_else(|| A::Error::invalid_length(index, &self))?;
                }
                if seq.next_element::<u8>()?.is_some() {
                    return Err(A::Error::invalid_length(129, &self));
                }
                Ok(bytes)
            }
        }
        deserializer.deserialize_tuple(128, Info)
    }
}

/// Backend observation after queuing or suppressing this child event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Disposition {
    /// Explicit SIG_IGN suppressed generation.
    Ignored,
    /// Pending, with the selected thread currently blocking SIGCHLD.
    PendingBlocked,
    /// Eligible for a Tool signal boundary; this alone does not promise guest EINTR.
    PendingEligible,
}
/// Why a backend rejected the event before changing pending state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    /// A declared unsupported operation.
    Unsupported,
    /// Invalid event metadata or identity.
    Invalid,
    /// An implementation failure.
    Backend,
}
/// Exact backend commit-stage result transported in the acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Outcome {
    /// The event was queued or explicitly ignored.
    Accepted {
        /// Signal disposition observed during insertion.
        disposition: Disposition,
        /// Existing pending/disposition generation, not a delivery identity.
        pending_generation: u64,
        /// An earlier standard signal already owned the pending metadata.
        coalesced: bool,
    },
    /// Pending state and readiness were not changed.
    RejectedBeforeCommit {
        /// Explicit classification of the refusal.
        kind: ErrorKind,
        /// Original numeric backend errno.
        errno: i32,
    },
    /// Pending state changed before a subsequent readiness operation failed.
    FailedAfterCommit {
        /// Original numeric backend errno; this delivery must not be retried.
        errno: i32,
        /// Generation in which pending insertion committed.
        pending_generation: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureStage {
    BeforeCommit,
    AfterCommit,
    Protocol,
    ReceiveAbandoned,
    CallbackAbandoned,
    UnsupportedRoute,
    ExitMismatch,
}

#[derive(Clone, Debug)]
pub(crate) struct Failure {
    pub operation: OperationId,
    pub delivery: u64,
    pub child: Option<DetPid>,
    pub errno: i32,
    pub stage: FailureStage,
    pub unsupported: bool,
    pub outcome: Option<Outcome>,
}

impl Failure {
    pub fn protocol(operation: OperationId, delivery: u64, child: DetPid) -> Self {
        let mut failure = Self::protocol_without_child(operation, delivery);
        failure.child = Some(child);
        failure
    }

    pub fn protocol_without_child(operation: OperationId, delivery: u64) -> Self {
        Self {
            operation,
            delivery,
            child: None,
            errno: libc::EPROTO,
            stage: FailureStage::Protocol,
            unsupported: false,
            outcome: None,
        }
    }

    pub fn from_outcome(command: &Command, outcome: Outcome) -> Option<Self> {
        let (stage, errno, unsupported) = match outcome {
            Outcome::Accepted { .. } => return None,
            Outcome::RejectedBeforeCommit { kind, errno } => (
                FailureStage::BeforeCommit,
                errno,
                kind == ErrorKind::Unsupported,
            ),
            Outcome::FailedAfterCommit { errno, .. } => (FailureStage::AfterCommit, errno, false),
        };
        Some(Self {
            operation: command.operation,
            delivery: command.delivery.id,
            child: Some(command.delivery.child),
            errno,
            stage,
            unsupported,
            outcome: Some(outcome),
        })
    }

    pub fn exit_status(&self) -> i32 {
        if self.unsupported {
            detcore_model::HERMIT_POLICY_REFUSAL_EXIT
        } else {
            detcore_model::HERMIT_INTERNAL_FAILURE_EXIT
        }
    }
}

/// Only fatal paths take this lock. No scheduler, operation, or backend lock is held
/// when entering termination; the competing failure participants use the same record.
#[derive(Debug)]
pub(crate) struct FatalRecord {
    pub failure: Failure,
    report: Mutex<()>,
}

impl FatalRecord {
    pub fn new(failure: Failure) -> Arc<Self> {
        Arc::new(Self {
            failure,
            report: Mutex::new(()),
        })
    }

    pub fn terminate(&self) -> ! {
        let _report = self
            .report
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let failure = &self.failure;
        let child = failure
            .child
            .map_or_else(|| "unavailable".to_owned(), |child| child.to_string());
        let message = format!(
            "HERMIT_CHILD_EXIT_FAILURE: exit={} tid={} mm={:?} operation={} delivery={} child={} errno={} stage={:?} outcome={:?}\n",
            failure.exit_status(),
            failure.operation.tid,
            failure.operation.mm,
            failure.operation.sequence,
            failure.delivery,
            child,
            failure.errno,
            failure.stage,
            failure.outcome
        );
        // Do not change O_NONBLOCK on stderr: that would change a shared open-file
        // description. A dedicated writer may block, but the terminating thread cannot.
        let fd = unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
        if fd >= 0 {
            // SAFETY: fcntl returned a new descriptor owned only by this writer.
            let mut stderr = unsafe { std::fs::File::from_raw_fd(fd) };
            let (sent, done) = std::sync::mpsc::sync_channel(1);
            if std::thread::Builder::new()
                .name("child-exit-failure".into())
                .spawn(move || {
                    let _ = stderr.write_all(message.as_bytes());
                    let _ = sent.send(());
                })
                .is_ok()
            {
                let _ = done.recv_timeout(Duration::from_millis(50));
            }
        }
        // This is a failed-run path. No guest grant, cleanup RPC, exit hook, Rust
        // destructor, or libc stdio flush may be required to terminate it.
        unsafe { libc::_exit(failure.exit_status()) }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ControlResult {
    Accepted,
    TargetRetired,
    Failed(Arc<FatalRecord>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptPhase {
    NotReturned,
    InCallback,
    TerminalAwaitingCallback,
    Settled,
}

#[derive(Debug)]
pub(crate) struct Attempt {
    pub command: Command,
    pub phase: AttemptPhase,
    pub acknowledgement: Ivar<ControlResult>,
    pub completion: Ivar<ControlResult>,
}

#[derive(Debug)]
pub(crate) enum Phase {
    Waiting,
    Delivering(Box<Attempt>),
    Granted,
    Retired,
    Failed(Arc<FatalRecord>),
}

#[derive(Debug)]
pub(crate) struct Operation {
    pub id: OperationId,
    pub parent: DetPid,
    pub original: Resources,
    pub guest_time: LogicalTime,
    pub phase: Mutex<Phase>,
}

impl Operation {
    pub fn retire(&self) {
        let mut phase = self.phase.lock().unwrap();
        match &mut *phase {
            Phase::Delivering(attempt) if attempt.phase != AttemptPhase::Settled => {
                // Retirement releases the scheduler's selection barrier immediately,
                // but completion remains pending until the exact callback settles.
                attempt
                    .acknowledgement
                    .try_put(ControlResult::TargetRetired);
                attempt.phase = AttemptPhase::TerminalAwaitingCallback;
            }
            Phase::Failed(_) => {}
            _ => *phase = Phase::Retired,
        }
    }

    pub fn awaiting_completion(&self) -> Option<Ivar<ControlResult>> {
        let phase = self.phase.lock().unwrap();
        match &*phase {
            Phase::Delivering(attempt)
                if attempt.phase == AttemptPhase::TerminalAwaitingCallback =>
            {
                Some(attempt.completion.clone())
            }
            _ => None,
        }
    }
}

/// This guard owns no Scheduler reference, so Drop cannot re-lock its mutex.
/// The operation mutex is acquired only in bounded synchronous blocks and is never
/// held across an await or while dropping a guard.
pub(crate) struct ReceiveGuard {
    operation: Option<Arc<Operation>>,
}
impl ReceiveGuard {
    pub fn new(operation: Option<Arc<Operation>>) -> Self {
        Self { operation }
    }
    pub fn disarm(&mut self) {
        self.operation = None;
    }
}
impl Drop for ReceiveGuard {
    fn drop(&mut self) {
        let Some(operation) = &self.operation else {
            return;
        };
        let fatal = {
            let mut phase = operation.phase.lock().unwrap();
            match &mut *phase {
                Phase::Delivering(attempt)
                    if attempt.phase == AttemptPhase::TerminalAwaitingCallback =>
                {
                    // No command escaped this receiver, so no backend operation can
                    // be in flight. Retire the unconsumed response synchronously.
                    attempt.completion.try_put(ControlResult::TargetRetired);
                    attempt.phase = AttemptPhase::Settled;
                    None
                }
                Phase::Delivering(attempt) => {
                    let mut failure = Failure::protocol(
                        operation.id,
                        attempt.command.delivery.id,
                        attempt.command.delivery.child,
                    );
                    failure.stage = FailureStage::ReceiveAbandoned;
                    let fatal = FatalRecord::new(failure);
                    attempt
                        .acknowledgement
                        .try_put(ControlResult::Failed(fatal.clone()));
                    attempt
                        .completion
                        .try_put(ControlResult::Failed(fatal.clone()));
                    *phase = Phase::Failed(fatal.clone());
                    Some(fatal)
                }
                _ => None,
            }
        };
        if let Some(fatal) = fatal {
            fatal.terminate();
        }
    }
}

pub(crate) struct CallbackGuard {
    command: Option<Command>,
}
impl CallbackGuard {
    pub fn new(command: Command) -> Self {
        Self {
            command: Some(command),
        }
    }
    pub fn disarm(&mut self) {
        self.command = None;
    }
}
impl Drop for CallbackGuard {
    fn drop(&mut self) {
        if let Some(command) = &self.command {
            let mut failure = Failure::protocol(
                command.operation,
                command.delivery.id,
                command.delivery.child,
            );
            failure.stage = FailureStage::CallbackAbandoned;
            FatalRecord::new(failure).terminate();
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct State {
    pub operations: HashMap<OperationId, Arc<Operation>>,
    pub current: BTreeMap<DetTid, OperationId>,
    pub sequences: BTreeMap<DetTid, u64>,
    pub normal_exits: HashMap<(DetPid, MmId), NormalExit>,
    pub pending: BTreeMap<u64, Delivery>,
    pub due: BTreeMap<u64, Delivery>,
    pub next_delivery: u64,
    pub fatal: Option<Arc<FatalRecord>>,
}

impl From<reverie::ChildExitSignalOutcome> for Outcome {
    fn from(value: reverie::ChildExitSignalOutcome) -> Self {
        use reverie::ChildExitSignalDisposition as D;
        use reverie::ChildExitSignalErrorKind as K;
        use reverie::ChildExitSignalOutcome as O;
        match value {
            O::Accepted {
                disposition,
                pending_generation,
                coalesced,
            } => Self::Accepted {
                disposition: match disposition {
                    D::Ignored => Disposition::Ignored,
                    D::PendingBlocked => Disposition::PendingBlocked,
                    D::PendingEligible => Disposition::PendingEligible,
                },
                pending_generation,
                coalesced,
            },
            O::RejectedBeforeCommit { kind, errno } => Self::RejectedBeforeCommit {
                kind: match kind {
                    K::Unsupported => ErrorKind::Unsupported,
                    K::Invalid => ErrorKind::Invalid,
                    K::Backend => ErrorKind::Backend,
                },
                errno: errno.into_raw(),
            },
            O::FailedAfterCommit {
                errno,
                pending_generation,
            } => Self::FailedAfterCommit {
                errno: errno.into_raw(),
                pending_generation,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;
    use std::process::Command as ProcessCommand;
    use std::process::Stdio;
    use std::time::Instant;

    use super::*;

    fn command() -> Command {
        let parent = DetPid::from_raw(3);
        Command::new(
            OperationId {
                tid: parent,
                mm: MmId::initial(parent),
                sequence: 9,
            },
            Delivery {
                id: 7,
                child: DetPid::from_raw(6),
                child_mm: MmId::initial(DetPid::from_raw(6)),
                parent,
                exit: NormalExit {
                    status: 37,
                    uid: 0,
                    user_ticks: 23,
                    system_ticks: 11,
                },
                deadline: LogicalTime::from_nanos(91),
            },
        )
    }

    #[test]
    fn exact_child_metadata_wire_and_backend_target() {
        let command = command();
        let value = serde_json::to_value(&command).unwrap();
        let decoded: Command = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded, command);
        let event = decoded
            .event(
                command.operation.tid,
                command.operation.mm,
                command.delivery.parent,
                reverie::Pid::from_raw(123_456),
            )
            .unwrap();
        assert_eq!(
            event.target(),
            reverie::SignalTarget::Process {
                pid: reverie::Pid::from_raw(123_456)
            }
        );
        assert_eq!(event.signal(), libc::SIGCHLD);
        let info = event.siginfo();
        assert_eq!(i32::from_ne_bytes(info[16..20].try_into().unwrap()), 6);
        assert_eq!(i32::from_ne_bytes(info[24..28].try_into().unwrap()), 37);
        assert_eq!(u64::from_ne_bytes(info[32..40].try_into().unwrap()), 23);
        assert_eq!(u64::from_ne_bytes(info[40..48].try_into().unwrap()), 11);
        for count in [0, 127, 129, 256] {
            let mut invalid = value.clone();
            invalid["siginfo"] = serde_json::json!(vec![0u8; count]);
            assert!(
                serde_json::from_value::<Command>(invalid).is_err(),
                "accepted {count} siginfo bytes"
            );
        }
        let mut padding = command.clone();
        padding.siginfo[127] = 99;
        assert_eq!(
            serde_json::from_slice::<Command>(&serde_json::to_vec(&padding).unwrap()).unwrap(),
            padding
        );
        assert!(
            padding
                .event(
                    command.operation.tid,
                    command.operation.mm,
                    command.delivery.parent,
                    reverie::Pid::from_raw(123_456)
                )
                .is_err(),
            "wire decoder or event reconstruction lost changed bytes"
        );
        for (tid, mm, parent, backend) in [
            (
                DetPid::from_raw(4),
                command.operation.mm,
                command.delivery.parent,
                123_456,
            ),
            (
                command.operation.tid,
                command.operation.mm.for_exec(command.operation.tid),
                command.delivery.parent,
                123_456,
            ),
            (
                command.operation.tid,
                command.operation.mm,
                DetPid::from_raw(8),
                123_456,
            ),
            (
                command.operation.tid,
                command.operation.mm,
                command.delivery.parent,
                0,
            ),
        ] {
            assert!(
                command
                    .event(tid, mm, parent, reverie::Pid::from_raw(backend))
                    .is_err()
            );
        }
        for field in ["unexpected", "target", "pid"] {
            let mut invalid = value.clone();
            invalid[field] = serde_json::json!(123);
            assert!(serde_json::from_value::<Command>(invalid).is_err());
        }
    }

    #[test]
    fn fatal_paths_terminate_with_full_stderr_and_preserve_descriptor_flags() {
        const CASE: &str = "HERMIT_TEST_CHILD_EXIT_FATAL_CASE";
        if let Ok(case) = std::env::var(CASE) {
            let command = command();
            if case == "receive-abandoned" {
                let operation = Arc::new(Operation {
                    id: command.operation,
                    parent: command.delivery.parent,
                    original: Resources::new(command.operation.tid),
                    guest_time: LogicalTime::ZERO,
                    phase: Mutex::new(Phase::Delivering(Box::new(Attempt {
                        command,
                        phase: AttemptPhase::NotReturned,
                        acknowledgement: Ivar::new(),
                        completion: Ivar::new(),
                    }))),
                });
                drop(ReceiveGuard::new(Some(operation)));
                panic!("abandoned receive returned instead of failing the run");
            }
            if case == "callback-abandoned" {
                drop(CallbackGuard::new(command));
                panic!("abandoned callback returned instead of failing the run");
            }
            let outcome = match case.as_str() {
                "unsupported" | "full-unsupported" => Outcome::RejectedBeforeCommit {
                    kind: ErrorKind::Unsupported,
                    errno: libc::ENOSYS,
                },
                "invalid" => Outcome::RejectedBeforeCommit {
                    kind: ErrorKind::Invalid,
                    errno: libc::EINVAL,
                },
                "backend" | "full-backend" => Outcome::RejectedBeforeCommit {
                    kind: ErrorKind::Backend,
                    errno: libc::EIO,
                },
                "postcommit" => Outcome::FailedAfterCommit {
                    errno: libc::EPIPE,
                    pending_generation: 43,
                },
                _ => panic!("unknown fatal child case"),
            };
            FatalRecord::new(Failure::from_outcome(&command, outcome).unwrap()).terminate();
        }
        for (case, status, errno, stage) in [
            ("unsupported", 122, libc::ENOSYS, "BeforeCommit"),
            ("invalid", 125, libc::EINVAL, "BeforeCommit"),
            ("backend", 125, libc::EIO, "BeforeCommit"),
            ("postcommit", 125, libc::EPIPE, "AfterCommit"),
            ("receive-abandoned", 125, libc::EPROTO, "ReceiveAbandoned"),
            ("callback-abandoned", 125, libc::EPROTO, "CallbackAbandoned"),
            ("full-unsupported", 122, libc::ENOSYS, "BeforeCommit"),
            ("full-backend", 125, libc::EIO, "BeforeCommit"),
        ] {
            let mut command = ProcessCommand::new(std::env::current_exe().unwrap());
            command.args(["--exact", "child_exit::tests::fatal_paths_terminate_with_full_stderr_and_preserve_descriptor_flags", "--nocapture", "--test-threads=1"])
                .env(CASE, case).stdin(Stdio::null()).stdout(Stdio::piped());
            let mut unread_pipe = None;
            let mut flags = None;
            if case.starts_with("full-") {
                let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
                let fd = write.as_raw_fd();
                let original = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                assert!(original >= 0);
                assert_eq!(
                    unsafe { libc::fcntl(fd, libc::F_SETFL, original | libc::O_NONBLOCK) },
                    0
                );
                let bytes = [b'x'; 4096];
                loop {
                    let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
                    if count < 0 {
                        assert_eq!(
                            std::io::Error::last_os_error().raw_os_error(),
                            Some(libc::EAGAIN)
                        );
                        break;
                    }
                }
                assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, original) }, 0);
                let stderr = write.try_clone().unwrap();
                command.stderr(Stdio::from(stderr));
                flags = Some((write, original));
                unread_pipe = Some(read);
            } else {
                command.stderr(Stdio::piped());
            }
            let start = Instant::now();
            let mut child = command.spawn().unwrap();
            let actual = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if start.elapsed() >= Duration::from_secs(2) {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{case}: fatal path waited for stderr, a hook, or an RPC");
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            assert_eq!(actual.code(), Some(status), "{case}");
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "{case}: termination exceeded its reporting interval"
            );
            if let Some((write, original)) = flags {
                assert_eq!(
                    unsafe { libc::fcntl(write.as_raw_fd(), libc::F_GETFL) },
                    original,
                    "fatal reporter changed shared stderr flags"
                );
            } else {
                let output = child.wait_with_output().unwrap();
                let stderr = String::from_utf8(output.stderr).unwrap();
                assert!(
                    stderr.contains(&format!("errno={errno} stage={stage}")),
                    "{case}: {stderr}"
                );
                assert!(
                    stderr.contains("operation=9 delivery=7 child=6"),
                    "{case}: {stderr}"
                );
                if case == "postcommit" {
                    assert!(stderr.contains("pending_generation: 43"));
                }
            }
            drop(unread_pipe);
        }
    }
}
