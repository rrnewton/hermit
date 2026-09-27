//! Provider registration receipts derived from authenticated controller transfers.
//! This does not register semantic task or FD ownership. The transport inbox
//! retains each pidfd, so a pidfs identity remains pinned for this receipt's life.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;

use super::AuxiliaryRole;
use super::CallStatus;
use super::CommandResult;
use super::Envelope;
use super::Identity;
use super::NetworkStreamOwner;
use super::Observation;
use super::Operation;
use super::ReceiveKind;
use super::Reply;
use super::Request;
use super::ffi;
use crate::types::DetTid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PidfdIdentity {
    device: u64,
    inode: u64,
}

impl PidfdIdentity {
    pub(crate) fn read(pidfd: &OwnedFd) -> io::Result<Self> {
        // Linux pidfs gives exact struct-pid identities on 64-bit systems. Do
        // not accept old anon_inode pidfds, whose inode is not a task identity.
        const PID_FS_MAGIC: libc::c_long = 0x50494446;
        let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstatfs(pidfd.as_raw_fd(), fs.as_mut_ptr()) } != 0
            || unsafe { libc::fstat(pidfd.as_raw_fd(), stat.as_mut_ptr()) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let fs = unsafe { fs.assume_init() };
        let stat = unsafe { stat.assume_init() };
        if fs.f_type != PID_FS_MAGIC || std::mem::size_of::<libc::ino_t>() != 8 {
            return Err(io::Error::other(
                "provider registration needs exact pidfs identity",
            ));
        }
        Ok(PidfdIdentity {
            device: stat.st_dev,
            inode: stat.st_ino,
        })
    }
}

struct Setter {
    identity: Identity,
    before: u64,
    after: u64,
    level: i32,
    option: i32,
}

struct AcceptPreparation {
    identity: Identity,
    lease: u64,
    mm: u64,
    fd: i32,
    flags: i32,
}
struct TablePreparation {
    registration: u64,
    mm: u64,
    expected_table: u64,
}
struct OriginalPreparation {
    kind: crate::network_replay::original_connect::Kind,
    call: u64,
    mm: u64,
    fd: i32,
    address: u64,
    length: i32,
    original_count: u64,
}
struct BirthPreparation {
    call: u64,
    mm: u64,
    table: u64,
    syscall: i32,
}
enum Preparation {
    Birth(BirthPreparation),
    Original(OriginalPreparation),
    Table(TablePreparation),
    Setter(Setter),
    Accept(AcceptPreparation),
}

trait Backend {
    type Pin;
    fn prepare_auxiliary_file(
        &mut self,
        _pin: &Self::Pin,
        _call: u64,
        _mm: u64,
        _fd: i32,
    ) -> io::Result<Observation<u64>> {
        Err(io::Error::other(
            "backend lacks explicit auxiliary file role",
        ))
    }
    fn prepare_helper_receive(
        &mut self,
        _pin: &Self::Pin,
        _call: u64,
        _mm: u64,
        _fd: i32,
        _role: AuxiliaryRole,
    ) -> io::Result<Observation<u64>> {
        Err(io::Error::other(
            "backend lacks explicit helper receive role",
        ))
    }
    fn terminate_birth(
        &mut self,
        _pin: &Self::Pin,
        _command: u64,
    ) -> io::Result<Observation<super::NativeBirthTerminal>> {
        Err(io::Error::other(
            "backend lacks exact dead-creator birth retirement",
        ))
    }
    fn cancel_birth(&mut self, _pin: &Self::Pin, _command: u64) -> io::Result<CallStatus> {
        Err(io::Error::other(
            "backend lacks known-uninvoked birth cancellation",
        ))
    }
    fn prepare_birth(
        &mut self,
        _pin: &Self::Pin,
        _request: BirthPreparation,
    ) -> io::Result<Observation<u64>> {
        Err(io::Error::other("backend lacks native birth issuer"))
    }
    fn observe_birth(
        &mut self,
        _child: &Self::Pin,
        _command: u64,
        _terminal: bool,
    ) -> io::Result<Observation<super::NativeBirth>> {
        Err(io::Error::other("backend lacks native birth observation"))
    }
    fn collect_birth(
        &mut self,
        _pin: &Self::Pin,
        _command: u64,
    ) -> io::Result<Observation<super::NativeBirthEffect>> {
        Err(io::Error::other("backend lacks native birth completion"))
    }
    fn prepare_original(
        &mut self,
        _pidfd: &Self::Pin,
        _request: OriginalPreparation,
    ) -> io::Result<Observation<u64>> {
        Err(io::Error::other(
            "backend lacks original connect entry receipt",
        ))
    }
    fn read_original(
        &mut self,
        _pidfd: &Self::Pin,
        _command: u64,
    ) -> io::Result<Observation<super::OriginalSelection>> {
        Err(io::Error::other("backend lacks original connect selection"))
    }
    fn read_original_control(
        &mut self,
        _pidfd: &Self::Pin,
        _command: u64,
    ) -> io::Result<Observation<super::OriginalResult>> {
        Err(io::Error::other(
            "backend lacks original epoll control selections",
        ))
    }
    fn collect_original(
        &mut self,
        _pidfd: &Self::Pin,
        _command: u64,
    ) -> io::Result<Observation<super::OriginalEffect>> {
        Err(io::Error::other(
            "backend lacks original connect completion",
        ))
    }
    fn cancel_original(&mut self, _pidfd: &Self::Pin, _command: u64) -> io::Result<CallStatus> {
        Err(io::Error::other(
            "backend lacks exact known-uninvoked disarm",
        ))
    }
    fn terminate_original(
        &mut self,
        _pidfd: &Self::Pin,
        _command: u64,
    ) -> io::Result<Observation<super::OriginalTerminal>> {
        Err(io::Error::other("backend lacks exact dead-task retirement"))
    }
    fn identity(&self, pidfd: &Self::Pin) -> io::Result<PidfdIdentity>;
    fn register(&mut self, pidfd: &Self::Pin) -> io::Result<CallStatus>;
    fn retire_auxiliary(&mut self, _pidfd: &Self::Pin) -> io::Result<CallStatus> {
        Err(io::Error::other(
            "backend lacks exact auxiliary registration retirement",
        ))
    }
    fn prepare(&mut self, pidfd: &Self::Pin, setter: Setter) -> io::Result<Observation<u64>>;
    fn finish(&mut self, pidfd: &Self::Pin, command: u64)
    -> io::Result<Observation<CommandResult>>;
    fn prepare_table(
        &mut self,
        _pidfd: &Self::Pin,
        _table: TablePreparation,
    ) -> io::Result<Observation<u64>> {
        Err(io::Error::other("backend lacks frozen table enrollment"))
    }
    fn collect_table(
        &mut self,
        _pidfd: &Self::Pin,
        _command: u64,
    ) -> io::Result<Observation<super::TableEnrollmentEffect>> {
        Err(io::Error::other("backend lacks frozen table collection"))
    }
    fn prepare_accept(
        &mut self,
        _pidfd: &Self::Pin,
        _accept: AcceptPreparation,
    ) -> io::Result<Observation<u64>> {
        Err(io::Error::other(
            "backend does not produce accepted installation receipts",
        ))
    }
    fn collect_accept(
        &mut self,
        _pidfd: &Self::Pin,
        _command: u64,
    ) -> io::Result<Observation<super::AcceptedEffect>> {
        Err(io::Error::other(
            "backend does not collect accepted installation receipts",
        ))
    }
}

struct Physical<'a>(&'a mut ffi::Session);
impl Backend for Physical<'_> {
    type Pin = OwnedFd;
    fn prepare_auxiliary_file(
        &mut self,
        pin: &OwnedFd,
        call: u64,
        mm: u64,
        fd: i32,
    ) -> io::Result<Observation<u64>> {
        Ok(self
            .0
            .prepare_auxiliary_file(pin.as_fd(), call, mm, fd)
            .into())
    }
    fn prepare_helper_receive(
        &mut self,
        pin: &OwnedFd,
        call: u64,
        mm: u64,
        fd: i32,
        role: AuxiliaryRole,
    ) -> io::Result<Observation<u64>> {
        let AuxiliaryRole::Receive {
            kind,
            address,
            count,
            ..
        } = role
        else {
            return Err(io::Error::other("receive backend given a non-receive role"));
        };
        if !role.valid() {
            return Err(io::Error::other("receive backend invalid helper operands"));
        }
        Ok(self
            .0
            .prepare_helper_receive(
                pin.as_fd(),
                call,
                mm,
                fd,
                address,
                count,
                kind == ReceiveKind::Peek,
            )
            .into())
    }
    fn prepare_original(
        &mut self,
        pidfd: &OwnedFd,
        r: OriginalPreparation,
    ) -> io::Result<Observation<u64>> {
        use crate::network_replay::original_connect::Kind;
        if !r.kind.valid_operands(r.address, r.length, r.original_count) {
            return Err(io::Error::other(
                "original preparation changed counted operands",
            ));
        }
        Ok(match r.kind {
            Kind::Socket => self.0.prepare_original_socket(
                pidfd.as_fd(),
                r.call,
                r.mm,
                r.fd,
                r.address as u32 as i32,
                r.length,
            ),
            Kind::EpollCreate { .. } => {
                self.0
                    .prepare_original_epoll(pidfd.as_fd(), r.call, r.mm, r.address as i32, r.fd)
            }
            Kind::Openat => self.0.prepare_original_openat(
                pidfd.as_fd(),
                r.call,
                r.mm,
                r.fd,
                r.address,
                r.length,
                r.original_count,
            ),
            Kind::Read => self.0.prepare_original_read(
                pidfd.as_fd(),
                r.call,
                r.mm,
                r.fd,
                r.address,
                r.original_count,
            ),
            Kind::EpollCtl => self.0.prepare_original_epoll_ctl(
                pidfd.as_fd(),
                r.call,
                r.mm,
                r.fd,
                r.length,
                r.original_count as u32 as i32,
                r.address,
            ),
            Kind::Connect => self.0.prepare_original_connect(
                pidfd.as_fd(),
                r.call,
                r.mm,
                r.fd,
                r.address,
                r.length,
            ),
            Kind::Close => {
                if r.address != 0 || r.length != 0 {
                    return Err(io::Error::other(
                        "close preparation contains connect operands",
                    ));
                }
                self.0
                    .prepare_original_close(pidfd.as_fd(), r.call, r.mm, r.fd)
            }
            Kind::File(operation) => {
                if r.address != operation.syscall() as u64 || r.length != operation.command() {
                    return Err(io::Error::other("file preparation changed syscall/command"));
                }
                self.0.prepare_original_file(
                    pidfd.as_fd(),
                    r.call,
                    r.mm,
                    r.fd,
                    operation.syscall() as i32,
                    operation.command(),
                )
            }
        }
        .into())
    }
    fn read_original(
        &mut self,
        pidfd: &OwnedFd,
        command: u64,
    ) -> io::Result<Observation<super::OriginalSelection>> {
        Ok(self
            .0
            .read_original_selection(pidfd.as_fd(), command)
            .into())
    }
    fn collect_original(
        &mut self,
        pidfd: &OwnedFd,
        command: u64,
    ) -> io::Result<Observation<super::OriginalEffect>> {
        Ok(self
            .0
            .collect_original_connect(pidfd.as_fd(), command)
            .into())
    }
    fn read_original_control(
        &mut self,
        pidfd: &OwnedFd,
        command: u64,
    ) -> io::Result<Observation<super::OriginalResult>> {
        Ok(self
            .0
            .read_original_epoll_ctl_selection(pidfd.as_fd(), command)
            .into())
    }
    fn cancel_original(&mut self, pidfd: &OwnedFd, command: u64) -> io::Result<CallStatus> {
        Ok(self
            .0
            .cancel_uninvoked_original(pidfd.as_fd(), command)
            .into())
    }
    fn terminate_original(
        &mut self,
        pidfd: &OwnedFd,
        command: u64,
    ) -> io::Result<Observation<super::OriginalTerminal>> {
        Ok(self.0.retire_dead_original(pidfd.as_fd(), command).into())
    }
    fn terminate_birth(
        &mut self,
        pin: &OwnedFd,
        command: u64,
    ) -> io::Result<Observation<super::NativeBirthTerminal>> {
        Ok(self.0.retire_dead_birth(pin.as_fd(), command).into())
    }
    fn prepare_birth(
        &mut self,
        pin: &OwnedFd,
        r: BirthPreparation,
    ) -> io::Result<Observation<u64>> {
        Ok(self
            .0
            .prepare_native_birth(pin.as_fd(), r.call, r.mm, r.table, r.syscall)
            .into())
    }
    fn cancel_birth(&mut self, pin: &Self::Pin, command: u64) -> io::Result<CallStatus> {
        Ok(self.0.cancel_uninvoked_birth(pin.as_fd(), command).into())
    }
    fn observe_birth(
        &mut self,
        child: &OwnedFd,
        command: u64,
        terminal: bool,
    ) -> io::Result<Observation<super::NativeBirth>> {
        Ok(if terminal {
            self.0.admit_native_birth_terminal(command)
        } else {
            self.0.admit_native_birth_child(child.as_fd(), command)
        }
        .into())
    }
    fn collect_birth(
        &mut self,
        pin: &OwnedFd,
        command: u64,
    ) -> io::Result<Observation<super::NativeBirthEffect>> {
        Ok(self.0.collect_native_birth(pin.as_fd(), command).into())
    }
    fn retire_auxiliary(&mut self, pidfd: &OwnedFd) -> io::Result<CallStatus> {
        Ok(self.0.retire_auxiliary_task(pidfd.as_fd()).into())
    }
    fn identity(&self, pidfd: &OwnedFd) -> io::Result<PidfdIdentity> {
        PidfdIdentity::read(pidfd)
    }
    fn register(&mut self, pidfd: &OwnedFd) -> io::Result<CallStatus> {
        Ok(self.0.register_task(pidfd.as_fd()).into())
    }
    fn prepare(&mut self, pidfd: &OwnedFd, setter: Setter) -> io::Result<Observation<u64>> {
        Ok(self
            .0
            .prepare_setter(
                pidfd.as_fd(),
                setter.identity.into(),
                setter.before,
                setter.after,
                setter.level,
                setter.option,
            )
            .into())
    }
    fn finish(&mut self, pidfd: &OwnedFd, command: u64) -> io::Result<Observation<CommandResult>> {
        Ok(self.0.finish_setter(pidfd.as_fd(), command).into())
    }
    fn prepare_table(
        &mut self,
        pidfd: &OwnedFd,
        table: TablePreparation,
    ) -> io::Result<Observation<u64>> {
        Ok(self
            .0
            .prepare_table_enrollment(
                pidfd.as_fd(),
                table.registration,
                table.mm,
                table.expected_table,
            )
            .into())
    }
    fn collect_table(
        &mut self,
        pidfd: &OwnedFd,
        command: u64,
    ) -> io::Result<Observation<super::TableEnrollmentEffect>> {
        Ok(self
            .0
            .collect_table_enrollment(pidfd.as_fd(), command)
            .into())
    }
    fn prepare_accept(
        &mut self,
        pidfd: &OwnedFd,
        accept: AcceptPreparation,
    ) -> io::Result<Observation<u64>> {
        Ok(self
            .0
            .prepare_accept(
                pidfd.as_fd(),
                accept.identity.into(),
                accept.lease,
                accept.mm,
                accept.fd,
                accept.flags,
            )
            .into())
    }
    fn collect_accept(
        &mut self,
        pidfd: &OwnedFd,
        command: u64,
    ) -> io::Result<Observation<super::AcceptedEffect>> {
        Ok(self.0.collect_accept(pidfd.as_fd(), command).into())
    }
}

struct Active {
    original_kind: Option<crate::network_replay::original_connect::Kind>,
    original_call: Option<u64>,
    operation: Operation,
    request: u64,
    command: Option<u64>,
    finish_submitted: bool,
    failed_finish: Option<u64>,
    birth_observation_submitted: bool,
}
/// One existing Call's physical operation on its retained NativeWorker.
/// The Inbox owns the exact PIDFD; this state never owns or recaptures the file.
struct Auxiliary {
    role: AuxiliaryRole,
    identity: PidfdIdentity,
    call: u64,
    request: u64,
    fd: i32,
    registration: Option<CallStatus>,
    command: Option<u64>,
    completion: Option<u64>,
    completion_known: bool,
    retirement_submitted: bool,
}
struct Registration {
    owner: NetworkStreamOwner,
    identity: PidfdIdentity,
    outcome: Option<CallStatus>,
    failure: Option<String>,
    active: Option<Active>,
    last_allocator: Option<super::OriginalEffect>,
    auxiliary: Option<Auxiliary>,
}

#[derive(Default)]
pub(super) struct Registrations(BTreeMap<DetTid, Registration>);
impl Registrations {
    pub(super) fn active_count(&self) -> usize {
        self.0
            .values()
            .filter(|entry| entry.active.is_some() || entry.auxiliary.is_some())
            .count()
    }

    pub(super) fn dispatch_physical(
        &mut self,
        session: &mut ffi::Session,
        envelope: &Envelope,
        rights: &[OwnedFd],
        prepared_rights: Option<&[OwnedFd]>,
    ) -> io::Result<Vec<u8>> {
        self.dispatch(&mut Physical(session), envelope, rights, prepared_rights)
    }

    // Both production and synthetic controls cross this request/rights decoder.
    // Only the C calls and exact held-pidfd query differ in the pure controls.
    fn dispatch<B: Backend>(
        &mut self,
        backend: &mut B,
        envelope: &Envelope,
        rights: &[B::Pin],
        prepared_rights: Option<&[B::Pin]>,
    ) -> io::Result<Vec<u8>> {
        let owner = envelope
            .owner
            .ok_or_else(|| io::Error::other("setter lacks task owner"))?;
        let request: Request = serde_json::from_slice(&envelope.body)?;
        let reply = match (envelope.operation, request) {
            (
                Operation::PrepareOriginalFileObservation,
                Request::PrepareOriginalFileObservation { call, mm, fd, role },
            ) if rights.len() == 1
                && envelope.accept.is_none()
                && call != 0
                && fd >= 0
                && mm == owner.mm.generation()
                && role.valid() =>
            {
                let observed = backend.identity(&rights[0])?;
                // The worker is an auxiliary actor, not a substituted guest.
                if self.0.values().any(|r| {
                    r.identity == observed
                        || r.auxiliary.as_ref().is_some_and(|a| a.identity == observed)
                }) {
                    return Err(io::Error::other(
                        "auxiliary worker aliases an existing task owner",
                    ));
                }
                let receipt = self
                    .0
                    .get_mut(&owner.thread)
                    .filter(|r| {
                        r.owner == owner
                            && r.active.is_none()
                            && r.auxiliary.is_none()
                            && r.failure.is_none()
                            && r.outcome
                                .as_ref()
                                .is_some_and(|s| s.returned == 0 && s.errno.is_none())
                    })
                    .ok_or_else(|| {
                        io::Error::other("auxiliary selection lost its original task registration")
                    })?;
                if role == AuxiliaryRole::File {
                    let original = receipt
                        .last_allocator
                        .as_ref()
                        .filter(|o| {
                            o.command.operation == 18
                                && o.original.selection.call == call
                                && o.original.selection.owner_mm == mm
                                && o.original.returned >= 0
                                && o.original.selection.file != 0
                                && o.original.complete == 1
                        })
                        .ok_or_else(|| {
                            io::Error::other(
                                "auxiliary selection lacks an actual Openat installation",
                            )
                        })?;
                    if original.command.command == 0 {
                        return Err(io::Error::other(
                            "auxiliary selection lost original command",
                        ));
                    }
                }
                // Receive Call authority belongs to the authenticated controller's
                // existing call owner. The C producer still requires the exact
                // tracked socket file at actual helper protocol entry; no helper
                // descriptor-table census or guest registration is manufactured.
                // Latch before either map mutation. An unknown result never
                // becomes a second register/prepare request.
                receipt.auxiliary = Some(Auxiliary {
                    role,
                    identity: observed,
                    call,
                    request: envelope.sequence,
                    fd,
                    registration: None,
                    command: None,
                    completion: None,
                    completion_known: false,
                    retirement_submitted: false,
                });
                let registration = backend.register(&rights[0])?;
                receipt.auxiliary.as_mut().unwrap().registration = Some(registration.clone());
                if registration.returned != 0 || registration.errno.is_some() {
                    Reply::Prepared(Observation {
                        status: registration,
                        raw: 0,
                    })
                } else {
                    let prepared = if role == AuxiliaryRole::File {
                        backend.prepare_auxiliary_file(&rights[0], call, mm, fd)?
                    } else {
                        backend.prepare_helper_receive(&rights[0], call, mm, fd, role)?
                    };
                    if prepared.status.returned == 0
                        && prepared.status.errno.is_none()
                        && prepared.raw != 0
                    {
                        receipt.auxiliary.as_mut().unwrap().command = Some(prepared.raw);
                    }
                    Reply::Prepared(prepared)
                }
            }
            (
                Operation::CollectOriginalFileObservation,
                Request::CollectOriginalFileObservation {
                    call,
                    command,
                    prepared_request,
                    role,
                },
            ) if rights.is_empty() && envelope.accept.is_none() => {
                let pins = prepared_rights.filter(|p| p.len() == 1).ok_or_else(|| {
                    io::Error::other("auxiliary collection lost original worker PIDFD")
                })?;
                let identity = backend.identity(&pins[0])?;
                let auxiliary = self
                    .0
                    .get_mut(&owner.thread)
                    .filter(|r| r.owner == owner)
                    .and_then(|r| r.auxiliary.as_mut())
                    .filter(|a| {
                        a.identity == identity
                            && a.call == call
                            && a.request == prepared_request
                            && a.role == role
                            && a.command == Some(command)
                            && a.completion.is_none()
                            && !a.retirement_submitted
                    })
                    .ok_or_else(|| {
                        io::Error::other("auxiliary collection changed its retained worker/command")
                    })?;
                auxiliary.completion = Some(envelope.sequence);
                let selection = backend.read_original(&pins[0], command)?;
                let effect = if selection.status.returned == 0 && selection.status.errno.is_none() {
                    role.check_selection(
                        &selection.raw,
                        call,
                        owner.mm.generation(),
                        auxiliary.fd,
                        command,
                    )?;
                    let observed = backend.collect_original(&pins[0], command)?;
                    if observed.status.returned == 0
                        && observed.status.errno.is_none()
                        && (observed.raw.command.operation != role.operation()
                            || observed.raw.original.selection != selection.raw)
                    {
                        return Err(io::Error::other(
                            "auxiliary completion changed explicit worker role",
                        ));
                    }
                    auxiliary.completion_known = observed.status.returned == 0
                        && observed.status.errno.is_none()
                        && observed.raw.original.complete == 1
                        && observed.raw.original.problem == 0;
                    Some(observed)
                } else {
                    None
                };
                Reply::OriginalFileObservation { selection, effect }
            }
            (
                Operation::RetireOriginalFileObservation,
                Request::RetireOriginalFileObservation {
                    call,
                    prepared,
                    completed,
                },
            ) if rights.is_empty() && envelope.accept.is_none() => {
                let pins = prepared_rights
                    .filter(|p| p.len() == 1)
                    .ok_or_else(|| io::Error::other("auxiliary retirement lost original PIDFD"))?;
                let identity = backend.identity(&pins[0])?;
                let receipt = self
                    .0
                    .get_mut(&owner.thread)
                    .filter(|r| r.owner == owner)
                    .ok_or_else(|| io::Error::other("auxiliary retirement lost original owner"))?;
                let auxiliary = receipt
                    .auxiliary
                    .as_mut()
                    .filter(|a| {
                        a.identity == identity
                            && a.call == call
                            && a.request == prepared
                            && a.completion == Some(completed)
                            && a.completion_known
                            && !a.retirement_submitted
                    })
                    .ok_or_else(|| {
                        io::Error::other("auxiliary retirement changed its exact group")
                    })?;
                // The service's incoming-group check requires the actual
                // original command ACK before this idle task-storage removal.
                auxiliary.retirement_submitted = true;
                let status = backend.retire_auxiliary(&pins[0])?;
                if status.returned == 0 && status.errno.is_none() {
                    receipt.auxiliary = None;
                }
                Reply::OriginalFileObservationRetired(status)
            }
            (
                operation @ (Operation::PrepareSetter
                | Operation::PrepareAccept
                | Operation::PrepareTableEnrollment
                | Operation::PrepareOriginalConnect
                | Operation::PrepareNativeBirth),
                request,
            ) if rights.len()
                == if matches!(
                    operation,
                    Operation::PrepareTableEnrollment
                        | Operation::PrepareOriginalConnect
                        | Operation::PrepareNativeBirth
                ) {
                    1
                } else {
                    2
                } =>
            {
                let preparation = match (operation, request) {
                    (
                        Operation::PrepareNativeBirth,
                        Request::PrepareNativeBirth {
                            call,
                            mm,
                            table,
                            syscall,
                        },
                    ) if call != 0
                        && table != 0
                        && matches!(syscall, 56 | 57 | 58 | 435)
                        && owner.mm.generation() == mm
                        && envelope.accept.is_none() =>
                    {
                        Preparation::Birth(BirthPreparation {
                            call,
                            mm,
                            table,
                            syscall,
                        })
                    }
                    (
                        Operation::PrepareOriginalConnect,
                        Request::PrepareOriginalConnect {
                            kind,
                            call,
                            mm,
                            fd,
                            address,
                            length,
                            original_count,
                        },
                    ) if call != 0 && owner.mm.generation() == mm && envelope.accept.is_none() => {
                        if kind == crate::network_replay::original_connect::Kind::Close
                            && (address != 0 || length != 0)
                        {
                            return Err(io::Error::other(
                                "close preparation changed zero operand contract",
                            ));
                        }
                        if let crate::network_replay::original_connect::Kind::File(operation) = kind
                            && (address != operation.syscall() as u64
                                || length != operation.command())
                        {
                            return Err(io::Error::other(
                                "file preparation changed scalar contract",
                            ));
                        }
                        if !kind.valid_operands(address, length, original_count) {
                            return Err(io::Error::other(
                                "original preparation changed counted operands",
                            ));
                        }
                        Preparation::Original(OriginalPreparation {
                            kind,
                            call,
                            mm,
                            fd,
                            address,
                            length,
                            original_count,
                        })
                    }
                    (
                        Operation::PrepareTableEnrollment,
                        Request::PrepareTableEnrollment {
                            registration,
                            mm,
                            expected_table,
                        },
                    ) if registration != 0
                        && owner.mm.generation() == mm
                        && envelope.accept.is_none() =>
                    {
                        Preparation::Table(TablePreparation {
                            registration,
                            mm,
                            expected_table,
                        })
                    }
                    (
                        Operation::PrepareSetter,
                        Request::PrepareSetter {
                            identity,
                            before,
                            after,
                            level,
                            option,
                        },
                    ) => Preparation::Setter(Setter {
                        identity,
                        before,
                        after,
                        level,
                        option,
                    }),
                    (
                        Operation::PrepareAccept,
                        Request::PrepareAccept {
                            identity,
                            lease,
                            mm,
                            fd,
                            flags,
                        },
                    ) if envelope.accept.map(|lease| lease.0) == Some(lease)
                        && owner.mm.generation() == mm =>
                    {
                        Preparation::Accept(AcceptPreparation {
                            identity,
                            lease,
                            mm,
                            fd,
                            flags,
                        })
                    }
                    _ => {
                        return Err(io::Error::other(
                            "provider preparation kind/owner/lease mismatch",
                        ));
                    }
                };
                let pidfd = &rights[if matches!(
                    operation,
                    Operation::PrepareTableEnrollment
                        | Operation::PrepareOriginalConnect
                        | Operation::PrepareNativeBirth
                ) {
                    0
                } else {
                    1
                }];
                let observed = backend.identity(pidfd)?;
                if self
                    .0
                    .values()
                    .any(|prior| prior.identity == observed && prior.owner != owner)
                {
                    return Err(io::Error::other(
                        "provider pidfd changed authenticated owner/MM",
                    ));
                }
                if let Some(prior) = self.0.get(&owner.thread) {
                    if prior.owner != owner || prior.identity != observed {
                        return Err(io::Error::other(
                            "provider task changed lifetime without retirement",
                        ));
                    }
                } else {
                    // Latch before C's BPF_NOEXIST mutation. Failure or unknown
                    // completion must not become another registration attempt.
                    self.0.insert(
                        owner.thread,
                        Registration {
                            owner,
                            identity: observed,
                            outcome: None,
                            failure: None,
                            active: None,
                            last_allocator: None,
                            auxiliary: None,
                        },
                    );
                    let receipt = self.0.get_mut(&owner.thread).unwrap();
                    match backend.register(pidfd) {
                        Ok(status) => receipt.outcome = Some(status),
                        Err(error) => {
                            receipt.failure = Some(error.to_string());
                            return Err(error);
                        }
                    }
                }
                let receipt = self.0.get_mut(&owner.thread).unwrap();
                let status = receipt.outcome.clone().ok_or_else(|| {
                    io::Error::other(
                        receipt
                            .failure
                            .clone()
                            .unwrap_or_else(|| "provider registration remains unknown".into()),
                    )
                })?;
                if status.returned != 0 {
                    Reply::Prepared(Observation { status, raw: 0 })
                } else {
                    if receipt.active.is_some() || receipt.auxiliary.is_some() {
                        return Err(io::Error::other(
                            "provider setter still has an unresolved command",
                        ));
                    }
                    receipt.active = Some(Active {
                        original_kind: match &preparation {
                            Preparation::Original(original) => Some(original.kind),
                            _ => None,
                        },
                        original_call: match &preparation {
                            Preparation::Original(r) => Some(r.call),
                            Preparation::Birth(r) => Some(r.call),
                            _ => None,
                        },
                        operation,
                        request: envelope.sequence,
                        command: None,
                        finish_submitted: false,
                        failed_finish: None,
                        birth_observation_submitted: false,
                    });
                    let outcome = match preparation {
                        Preparation::Birth(request) => backend.prepare_birth(pidfd, request)?,
                        Preparation::Original(request) => {
                            backend.prepare_original(pidfd, request)?
                        }
                        Preparation::Table(table) => backend.prepare_table(pidfd, table)?,
                        Preparation::Setter(setter) => backend.prepare(pidfd, setter)?,
                        Preparation::Accept(accept) => backend.prepare_accept(pidfd, accept)?,
                    };
                    if outcome.status.returned == 0 && outcome.raw != 0 {
                        receipt.active.as_mut().unwrap().command = Some(outcome.raw);
                    }
                    Reply::Prepared(outcome)
                }
            }
            (
                Operation::ObserveNativeBirth,
                Request::ObserveNativeBirth {
                    call,
                    command,
                    prepared_request,
                    child: child_tid,
                    terminal,
                },
            ) if child_tid > 0 && rights.len() == 1 && envelope.accept.is_none() => {
                let pins = prepared_rights
                    .filter(|pins| pins.len() == 1)
                    .ok_or_else(|| io::Error::other("birth lost retained creator"))?;
                let creator = backend.identity(&pins[0])?;
                let child = backend.identity(&rights[0])?;
                if creator == child {
                    return Err(io::Error::other("birth child equals creator"));
                }
                let active = self
                    .0
                    .get_mut(&owner.thread)
                    .filter(|r| r.owner == owner && r.identity == creator)
                    .and_then(|r| r.active.as_mut())
                    .filter(|a| {
                        a.operation == Operation::PrepareNativeBirth
                            && a.request == prepared_request
                            && a.command == Some(command)
                            && a.original_call == Some(call)
                            && !a.finish_submitted
                    })
                    .ok_or_else(|| io::Error::other("birth changed retained invocation"))?;
                if active.birth_observation_submitted {
                    return Err(io::Error::other(
                        "birth admission remains owned by its first request",
                    ));
                }
                // Latch before the live marker mutation. An exception/unknown
                // result never becomes a second physical admission attempt.
                active.birth_observation_submitted = true;
                let observation = backend.observe_birth(&rights[0], command, terminal)?;
                if observation.status.returned == 0
                    && (observation.raw.command != command || observation.raw.call != call)
                {
                    return Err(io::Error::other("birth returned a different operation"));
                }
                Reply::NativeBirth(observation)
            }
            (
                Operation::AwaitOriginalSelection,
                Request::AwaitOriginalSelection {
                    call,
                    command,
                    prepared_request,
                },
            ) if rights.is_empty() && envelope.accept.is_none() => {
                let pins = prepared_rights
                    .filter(|pins| pins.len() == 1)
                    .ok_or_else(|| io::Error::other("original selection lacks retained target"))?;
                let identity = backend.identity(&pins[0])?;
                let active = self
                    .0
                    .get(&owner.thread)
                    .filter(|r| r.owner == owner && r.identity == identity)
                    .and_then(|r| r.active.as_ref())
                    .filter(|a| {
                        a.operation == Operation::PrepareOriginalConnect
                            && a.request == prepared_request
                            && a.command == Some(command)
                            && a.original_call == Some(call)
                            && !a.finish_submitted
                    })
                    .ok_or_else(|| {
                        io::Error::other("original selection changed its admitted invocation")
                    })?;
                if active.original_kind
                    == Some(crate::network_replay::original_connect::Kind::EpollCtl)
                {
                    let observation = backend.read_original_control(&pins[0], command)?;
                    if observation.status.returned == 0
                        && (observation.status.errno.is_some()
                            || observation.raw.selection.command != command
                            || observation.raw.selection.call != call)
                    {
                        return Err(io::Error::other(
                            "original control selection changed its completed identity",
                        ));
                    }
                    Reply::OriginalControlSelection(observation)
                } else {
                    let observation = backend.read_original(&pins[0], command)?;
                    if observation.status.returned == 0
                        && (observation.status.errno.is_some()
                            || observation.raw.command != command
                            || observation.raw.call != call)
                    {
                        return Err(io::Error::other(
                            "original selection changed its completed identity",
                        ));
                    }
                    Reply::OriginalSelection(observation)
                }
            }
            (
                operation @ (Operation::FinishSetter
                | Operation::CollectAccept
                | Operation::CollectTableEnrollment
                | Operation::CollectOriginalConnect
                | Operation::CollectNativeBirth
                | Operation::CancelNativeBirth
                | Operation::TerminateNativeBirth
                | Operation::CancelOriginalConnect
                | Operation::TerminateOriginalConnect),
                request,
            ) if rights.is_empty() => {
                let original_kind = match &request {
                    Request::CollectOriginalConnect { kind, .. } => Some(*kind),
                    _ => None,
                };
                let terminal_failed = match &request {
                    Request::TerminateOriginalConnect { failed_request, .. } => *failed_request,
                    _ => None,
                };
                let original_call = match &request {
                    Request::TerminateNativeBirth { call, .. }
                    | Request::CancelNativeBirth { call, .. }
                    | Request::CollectNativeBirth { call, .. }
                    | Request::CollectOriginalConnect { call, .. }
                    | Request::CancelOriginalConnect { call, .. }
                    | Request::TerminateOriginalConnect { call, .. } => Some(*call),
                    _ => None,
                };
                let (command, prepared_request, preparation) = match (operation, request) {
                    (
                        Operation::TerminateNativeBirth,
                        Request::TerminateNativeBirth {
                            command,
                            prepared_request,
                            ..
                        },
                    ) => (command, prepared_request, Operation::PrepareNativeBirth),
                    (
                        Operation::CancelNativeBirth,
                        Request::CancelNativeBirth {
                            command,
                            prepared_request,
                            ..
                        },
                    ) => (command, prepared_request, Operation::PrepareNativeBirth),
                    (
                        Operation::CollectNativeBirth,
                        Request::CollectNativeBirth {
                            command,
                            prepared_request,
                            ..
                        },
                    ) => (command, prepared_request, Operation::PrepareNativeBirth),
                    (
                        Operation::TerminateOriginalConnect,
                        Request::TerminateOriginalConnect {
                            command,
                            prepared_request,
                            ..
                        },
                    ) => (command, prepared_request, Operation::PrepareOriginalConnect),
                    (
                        Operation::CancelOriginalConnect,
                        Request::CancelOriginalConnect {
                            command,
                            prepared_request,
                            ..
                        },
                    ) => (command, prepared_request, Operation::PrepareOriginalConnect),
                    (
                        Operation::CollectOriginalConnect,
                        Request::CollectOriginalConnect {
                            command,
                            prepared_request,
                            ..
                        },
                    ) => (command, prepared_request, Operation::PrepareOriginalConnect),
                    (
                        Operation::CollectTableEnrollment,
                        Request::CollectTableEnrollment {
                            command,
                            prepared_request,
                        },
                    ) => (command, prepared_request, Operation::PrepareTableEnrollment),
                    (
                        Operation::FinishSetter,
                        Request::FinishSetter {
                            command,
                            prepared_request,
                        },
                    ) => (command, prepared_request, Operation::PrepareSetter),
                    (
                        Operation::CollectAccept,
                        Request::CollectAccept {
                            command,
                            prepared_request,
                        },
                    ) => (command, prepared_request, Operation::PrepareAccept),
                    _ => return Err(io::Error::other("provider completion kind mismatch")),
                };
                let table = matches!(
                    preparation,
                    Operation::PrepareTableEnrollment
                        | Operation::PrepareOriginalConnect
                        | Operation::PrepareNativeBirth
                );
                let pins = prepared_rights
                    .filter(|pins| pins.len() == if table { 1 } else { 2 })
                    .ok_or_else(|| io::Error::other("setter finish lacks retained preparation"))?;
                let pin = &pins[if table { 0 } else { 1 }];
                let identity = backend.identity(pin)?;
                let receipt = self
                    .0
                    .get_mut(&owner.thread)
                    .filter(|receipt| receipt.owner == owner && receipt.identity == identity)
                    .ok_or_else(|| io::Error::other("setter finish changed task lifetime"))?;
                let active = receipt
                    .active
                    .as_mut()
                    .filter(|active| {
                        active.request == prepared_request
                            && active.command == Some(command)
                            && active.operation == preparation
                            && active.original_call == original_call
                            && original_kind.is_none_or(|kind| active.original_kind == Some(kind))
                    })
                    .ok_or_else(|| io::Error::other("setter finish changed active command"))?;
                if active.finish_submitted
                    && !(operation == Operation::TerminateOriginalConnect
                        && terminal_failed.is_some()
                        && active.failed_finish == terminal_failed)
                {
                    return Err(io::Error::other(
                        "setter finish remains submitted; reuse its retained transport reply",
                    ));
                }
                active.finish_submitted = true;
                let reply = match operation {
                    Operation::TerminateOriginalConnect => {
                        Reply::OriginalTerminated(backend.terminate_original(pin, command)?)
                    }
                    Operation::TerminateNativeBirth => {
                        Reply::NativeBirthTerminated(backend.terminate_birth(pin, command)?)
                    }
                    Operation::CancelNativeBirth => Reply::NativeBirthCanceled {
                        command,
                        status: backend.cancel_birth(pin, command)?,
                    },
                    Operation::CollectNativeBirth => {
                        Reply::NativeBirthEffect(backend.collect_birth(pin, command)?)
                    }
                    Operation::CollectOriginalConnect => {
                        Reply::OriginalEffect(backend.collect_original(pin, command)?)
                    }
                    Operation::CancelOriginalConnect => Reply::OriginalCanceled {
                        command,
                        status: backend.cancel_original(pin, command)?,
                    },
                    Operation::CollectTableEnrollment => {
                        Reply::TableEnrollmentEffect(backend.collect_table(pin, command)?)
                    }
                    Operation::FinishSetter => Reply::Command(backend.finish(&pins[1], command)?),
                    Operation::CollectAccept => {
                        Reply::AcceptedEffect(backend.collect_accept(&pins[1], command)?)
                    }
                    _ => unreachable!(),
                };
                let (status, returned_command) = match &reply {
                    Reply::OriginalTerminated(outcome) => {
                        (&outcome.status, outcome.raw.command.command)
                    }
                    Reply::NativeBirthTerminated(outcome) => {
                        (&outcome.status, outcome.raw.command.command)
                    }
                    Reply::NativeBirthCanceled { command, status } => (status, *command),
                    Reply::NativeBirthEffect(outcome) => {
                        (&outcome.status, outcome.raw.command.command)
                    }
                    Reply::OriginalEffect(outcome) => {
                        (&outcome.status, outcome.raw.command.command)
                    }
                    Reply::OriginalCanceled { command, status } => (status, *command),
                    Reply::TableEnrollmentEffect(outcome) => {
                        (&outcome.status, outcome.raw.command.command)
                    }
                    Reply::Command(outcome) => (&outcome.status, outcome.raw.command),
                    Reply::AcceptedEffect(outcome) => {
                        (&outcome.status, outcome.raw.command.command)
                    }
                    _ => unreachable!(),
                };
                if status.returned == 0 {
                    if returned_command != command {
                        return Err(io::Error::other("provider finish returned another command"));
                    }
                    if let Reply::OriginalEffect(observed) = &reply {
                        if matches!(
                            original_kind,
                            Some(
                                crate::network_replay::original_connect::Kind::Socket
                                    | crate::network_replay::original_connect::Kind::Openat
                                    | crate::network_replay::original_connect::Kind::EpollCreate { .. }
                            )
                        ) {
                            receipt.last_allocator = Some(observed.raw.clone());
                        }
                    }
                    receipt.active = None;
                } else {
                    receipt.active.as_mut().unwrap().failed_finish = Some(envelope.sequence);
                }
                reply
            }
            _ => {
                return Err(io::Error::other(
                    "provider setter request/rights/phase mismatch",
                ));
            }
        };
        serde_json::to_vec(&reply).map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        registrations: usize,
        preparations: usize,
        completions: usize,
        registration_errno: Option<i32>,
        registration_unknown: bool,
        preparation_unknown: bool,
        finish_unknown: bool,
        original_errno: Option<i32>,
        original_returned_command: Option<u64>,
        terminal_ready: bool,
        terminal_calls: usize,
    }
    fn status(operation: &str) -> CallStatus {
        CallStatus {
            operation: operation.into(),
            returned: 0,
            errno: None,
        }
    }
    impl Backend for Fake {
        type Pin = PidfdIdentity;
        fn terminate_original(
            &mut self,
            _: &Self::Pin,
            command: u64,
        ) -> io::Result<Observation<super::super::OriginalTerminal>> {
            self.terminal_calls += 1;
            let status = if self.terminal_ready {
                status("ap_retire_dead_original")
            } else {
                CallStatus {
                    operation: "ap_retire_dead_original".into(),
                    returned: -1,
                    errno: Some(libc::EAGAIN),
                }
            };
            Ok(Observation {
                status,
                raw: ffi::OriginalTerminal {
                    command: ffi::CommandResult {
                        command,
                        operation: 7,
                        ..Default::default()
                    },
                    call: 17,
                    task_absent: u64::from(self.terminal_ready),
                    ..Default::default()
                }
                .into(),
            })
        }
        fn prepare_original(
            &mut self,
            _: &Self::Pin,
            request: OriginalPreparation,
        ) -> io::Result<Observation<u64>> {
            assert_eq!((request.call, request.mm), (17, owner().mm.generation()));
            assert_eq!((request.fd, request.address, request.length), (6, 4096, 24));
            self.preparations += 1;
            Ok(Observation {
                status: status("prepare_original"),
                raw: self.preparations as u64,
            })
        }
        fn collect_original(
            &mut self,
            _: &Self::Pin,
            command: u64,
        ) -> io::Result<Observation<super::super::OriginalEffect>> {
            let status = self.original_completion_status()?;
            Ok(Observation {
                status,
                raw: ffi::OriginalEffect {
                    command: ffi::CommandResult {
                        command: self.original_returned_command.unwrap_or(command),
                        operation: 7,
                        ..Default::default()
                    },
                    ..Default::default()
                }
                .into(),
            })
        }
        fn cancel_original(&mut self, _: &Self::Pin, command: u64) -> io::Result<CallStatus> {
            assert_eq!(command, 1);
            self.original_completion_status()
        }
        fn identity(&self, pin: &Self::Pin) -> io::Result<PidfdIdentity> {
            Ok(*pin)
        }
        fn register(&mut self, _: &Self::Pin) -> io::Result<CallStatus> {
            self.registrations += 1;
            if self.registration_unknown {
                return Err(io::Error::other("registration mutated before failure"));
            }
            Ok(match self.registration_errno {
                Some(errno) => CallStatus {
                    operation: "register".into(),
                    returned: -1,
                    errno: Some(errno),
                },
                None => status("register"),
            })
        }
        fn prepare(&mut self, _: &Self::Pin, setter: Setter) -> io::Result<Observation<u64>> {
            assert_eq!(setter.identity.object, 9);
            assert_eq!(setter.after, setter.before + 1);
            assert_eq!(setter.level, libc::SOL_SOCKET);
            assert_eq!(setter.option, libc::SO_RCVLOWAT);
            self.preparations += 1;
            if self.preparation_unknown {
                return Err(io::Error::other("command submitted before failure"));
            }
            Ok(Observation {
                status: status("prepare"),
                raw: self.preparations as u64,
            })
        }
        fn finish(
            &mut self,
            _: &Self::Pin,
            command: u64,
        ) -> io::Result<Observation<CommandResult>> {
            self.completions += 1;
            if self.finish_unknown {
                return Err(io::Error::other("finish effect has unknown result"));
            }
            Ok(Observation {
                status: status("finish"),
                raw: ffi::CommandResult {
                    command,
                    ..Default::default()
                }
                .into(),
            })
        }
    }
    impl Fake {
        fn original_completion_status(&mut self) -> io::Result<CallStatus> {
            self.completions += 1;
            if self.finish_unknown {
                return Err(io::Error::other("original completion has unknown result"));
            }
            Ok(match self.original_errno {
                Some(errno) => CallStatus {
                    operation: "original_completion".into(),
                    returned: -1,
                    errno: Some(errno),
                },
                None => status("original_completion"),
            })
        }
    }
    fn owner() -> NetworkStreamOwner {
        let thread = DetTid::from_raw(41);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    fn pins() -> [PidfdIdentity; 2] {
        [
            PidfdIdentity {
                device: 2,
                inode: 70,
            },
            PidfdIdentity {
                device: 3,
                inode: 71,
            },
        ]
    }
    fn prepare(sequence: u64, owner: NetworkStreamOwner) -> Envelope {
        Envelope {
            run: [3; 16],
            sequence,
            owner: Some(owner),
            accept: None,
            operation: Operation::PrepareSetter,
            body: serde_json::to_vec(&Request::PrepareSetter {
                identity: Identity {
                    provider: 1,
                    object: 9,
                    namespace: 2,
                },
                before: sequence,
                after: sequence + 1,
                level: libc::SOL_SOCKET,
                option: libc::SO_RCVLOWAT,
            })
            .unwrap(),
        }
    }
    fn finish(
        sequence: u64,
        prepared_request: u64,
        command: u64,
        owner: NetworkStreamOwner,
    ) -> Envelope {
        Envelope {
            run: [3; 16],
            sequence,
            owner: Some(owner),
            accept: None,
            operation: Operation::FinishSetter,
            body: serde_json::to_vec(&Request::FinishSetter {
                command,
                prepared_request,
            })
            .unwrap(),
        }
    }
    fn prepared(bytes: Vec<u8>) -> Observation<u64> {
        let Reply::Prepared(value) = serde_json::from_slice(&bytes).unwrap() else {
            panic!("not preparation")
        };
        value
    }

    fn prepare_original() -> Envelope {
        let mut envelope = prepare(1, owner());
        envelope.operation = Operation::PrepareOriginalConnect;
        envelope.body = serde_json::to_vec(&Request::PrepareOriginalConnect {
            kind: crate::network_replay::original_connect::Kind::Connect,
            call: 17,
            mm: owner().mm.generation(),
            fd: 6,
            address: 4096,
            length: 24,
            original_count: 0,
        })
        .unwrap();
        envelope
    }
    fn finish_original(cancel: bool, command: u64) -> Envelope {
        let mut envelope = finish(3, 1, command, owner());
        let request = if cancel {
            envelope.operation = Operation::CancelOriginalConnect;
            Request::CancelOriginalConnect {
                call: 17,
                command,
                prepared_request: 1,
                selected_request: 2,
            }
        } else {
            envelope.operation = Operation::CollectOriginalConnect;
            Request::CollectOriginalConnect {
                kind: crate::network_replay::original_connect::Kind::Connect,
                call: 17,
                command,
                prepared_request: 1,
            }
        };
        envelope.body = serde_json::to_vec(&request).unwrap();
        envelope
    }
    #[test]
    fn original_dispatch_collect_and_disarm_retire_only_their_exact_active_command() {
        for cancel in [false, true] {
            let mut registrations = Registrations::default();
            let mut backend = Fake::default();
            let pin = [pins()[1]];
            let ready = prepared(
                registrations
                    .dispatch(&mut backend, &prepare_original(), &pin, None)
                    .unwrap(),
            );
            assert_eq!(ready.raw, 1);
            assert_eq!(registrations.active_count(), 1);
            let completed = registrations
                .dispatch(&mut backend, &finish_original(cancel, 1), &[], Some(&pin))
                .unwrap();
            match serde_json::from_slice::<Reply>(&completed).unwrap() {
                Reply::OriginalEffect(outcome) if !cancel => {
                    assert_eq!(outcome.status.returned, 0);
                    assert_eq!(outcome.raw.command.command, 1);
                }
                Reply::OriginalCanceled { command, status } if cancel => {
                    assert_eq!(command, 1);
                    assert_eq!(status.returned, 0);
                }
                other => panic!("original completion changed its type: {other:?}"),
            }
            assert_eq!(registrations.active_count(), 0);
            assert!(
                registrations
                    .dispatch(&mut backend, &finish_original(cancel, 1), &[], Some(&pin))
                    .is_err()
            );
            assert_eq!(
                (
                    backend.registrations,
                    backend.preparations,
                    backend.completions
                ),
                (1, 1, 1)
            );
        }
    }
    #[test]
    fn original_dispatch_changed_command_preserves_active_custody() {
        for cancel in [false, true] {
            let mut registrations = Registrations::default();
            let mut backend = Fake::default();
            let pin = [pins()[1]];
            registrations
                .dispatch(&mut backend, &prepare_original(), &pin, None)
                .unwrap();
            assert!(
                registrations
                    .dispatch(&mut backend, &finish_original(cancel, 2), &[], Some(&pin))
                    .is_err()
            );
            assert_eq!(backend.completions, 0);
            let active = registrations.0[&owner().thread].active.as_ref().unwrap();
            assert_eq!(active.command, Some(1));
            assert!(!active.finish_submitted);
            if !cancel {
                backend.original_returned_command = Some(2);
                let error = registrations
                    .dispatch(&mut backend, &finish_original(false, 1), &[], Some(&pin))
                    .unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "provider finish returned another command"
                );
                assert_eq!(backend.completions, 1);
                let active = registrations.0[&owner().thread].active.as_ref().unwrap();
                assert_eq!(active.command, Some(1));
                assert!(active.finish_submitted);
            }
            assert_eq!(registrations.active_count(), 1);
        }
    }
    #[test]
    fn original_dispatch_failed_or_unknown_collect_and_disarm_never_reopen_custody() {
        for cancel in [false, true] {
            for unknown in [false, true] {
                let mut registrations = Registrations::default();
                let mut backend = Fake {
                    finish_unknown: unknown,
                    original_errno: Some(libc::EIO),
                    ..Default::default()
                };
                let pin = [pins()[1]];
                registrations
                    .dispatch(&mut backend, &prepare_original(), &pin, None)
                    .unwrap();
                let outcome = registrations.dispatch(
                    &mut backend,
                    &finish_original(cancel, 1),
                    &[],
                    Some(&pin),
                );
                if unknown {
                    assert!(outcome.is_err());
                } else {
                    let status = match serde_json::from_slice::<Reply>(&outcome.unwrap()).unwrap() {
                        Reply::OriginalEffect(outcome) if !cancel => outcome.status,
                        Reply::OriginalCanceled { command: 1, status } if cancel => status,
                        other => panic!("original failure changed its type: {other:?}"),
                    };
                    assert_eq!((status.returned, status.errno), (-1, Some(libc::EIO)));
                }
                let active = registrations.0[&owner().thread].active.as_ref().unwrap();
                assert_eq!(active.command, Some(1));
                assert!(active.finish_submitted);
                assert_eq!(registrations.active_count(), 1);
                assert!(
                    registrations
                        .dispatch(&mut backend, &finish_original(cancel, 1), &[], Some(&pin))
                        .is_err()
                );
                assert!(
                    registrations
                        .dispatch(&mut backend, &prepare_original(), &pin, None)
                        .is_err()
                );
                assert_eq!(
                    (
                        backend.registrations,
                        backend.preparations,
                        backend.completions
                    ),
                    (1, 1, 1)
                );
            }
        }
    }

    #[test]
    fn original_dead_dispatch_keeps_live_target_and_failed_collection_custody_distinct() {
        for failed in [false, true] {
            let mut registrations = Registrations::default();
            let mut backend = Fake {
                original_errno: Some(libc::ENODATA),
                ..Default::default()
            };
            let pin = [pins()[1]];
            registrations
                .dispatch(&mut backend, &prepare_original(), &pin, None)
                .unwrap();
            if failed {
                let body = registrations
                    .dispatch(&mut backend, &finish_original(false, 1), &[], Some(&pin))
                    .unwrap();
                assert!(
                    matches!(serde_json::from_slice::<Reply>(&body).unwrap(),Reply::OriginalEffect(out) if out.status.returned==-1)
                );
            }
            let mut terminal = finish_original(false, 1);
            terminal.sequence = 4;
            terminal.operation = Operation::TerminateOriginalConnect;
            terminal.body = serde_json::to_vec(&Request::TerminateOriginalConnect {
                call: 17,
                command: 1,
                prepared_request: 1,
                selected_request: 2,
                failed_request: failed.then_some(3),
            })
            .unwrap();
            let wrong = [PidfdIdentity {
                inode: pin[0].inode + 1,
                ..pin[0]
            }];
            assert!(
                registrations
                    .dispatch(&mut backend, &terminal, &[], Some(&wrong))
                    .is_err()
            );
            assert_eq!(backend.terminal_calls, 0);
            if failed {
                let mut wrong = terminal.clone();
                wrong.body = serde_json::to_vec(&Request::TerminateOriginalConnect {
                    call: 17,
                    command: 1,
                    prepared_request: 1,
                    selected_request: 2,
                    failed_request: Some(99),
                })
                .unwrap();
                assert!(
                    registrations
                        .dispatch(&mut backend, &wrong, &[], Some(&pin))
                        .is_err()
                );
                assert_eq!(backend.terminal_calls, 0);
            }
            backend.terminal_ready = failed;
            let body = registrations
                .dispatch(&mut backend, &terminal, &[], Some(&pin))
                .unwrap();
            let Reply::OriginalTerminated(out) = serde_json::from_slice(&body).unwrap() else {
                panic!("wrong terminal type")
            };
            assert_eq!(out.raw.command.command, 1);
            assert_eq!(out.status.returned, if failed { 0 } else { -1 });
            assert_eq!(registrations.active_count(), usize::from(!failed));
            assert_eq!(backend.terminal_calls, 1);
            assert!(
                registrations
                    .dispatch(&mut backend, &terminal, &[], Some(&pin))
                    .is_err()
            );
            assert_eq!(backend.terminal_calls, 1);
        }
    }

    #[test]
    fn accepted_setter_dispatch_registers_exact_task_once_for_two_completed_commands() {
        let mut receipts = Registrations::default();
        let mut backend = Fake::default();
        let first = prepared(
            receipts
                .dispatch(&mut backend, &prepare(1, owner()), &pins(), None)
                .unwrap(),
        );
        assert_eq!(first.status.returned, 0);
        assert_eq!(first.raw, 1);
        let completed = receipts
            .dispatch(
                &mut backend,
                &finish(2, 1, first.raw, owner()),
                &[],
                Some(&pins()),
            )
            .unwrap();
        let Reply::Command(completed) = serde_json::from_slice(&completed).unwrap() else {
            panic!("not completion")
        };
        assert_eq!(completed.status.returned, 0);
        assert_eq!(completed.raw.command, 1);
        let second = prepared(
            receipts
                .dispatch(&mut backend, &prepare(3, owner()), &pins(), None)
                .unwrap(),
        );
        assert_eq!(second.status.returned, 0);
        assert_eq!(second.raw, 2);
        receipts
            .dispatch(
                &mut backend,
                &finish(4, 3, second.raw, owner()),
                &[],
                Some(&pins()),
            )
            .unwrap();
        assert_eq!(
            (
                backend.registrations,
                backend.preparations,
                backend.completions
            ),
            (1, 2, 2)
        );
    }

    #[test]
    fn accepted_setter_dispatch_rejects_changed_owner_or_lifetime_before_any_new_effect() {
        let mut receipts = Registrations::default();
        let mut backend = Fake::default();
        receipts
            .dispatch(&mut backend, &prepare(1, owner()), &pins(), None)
            .unwrap();
        receipts
            .dispatch(&mut backend, &finish(2, 1, 1, owner()), &[], Some(&pins()))
            .unwrap();
        let changed_mm = NetworkStreamOwner {
            mm: owner().mm.for_exec(owner().thread),
            ..owner()
        };
        assert!(
            receipts
                .dispatch(&mut backend, &prepare(3, changed_mm), &pins(), None)
                .is_err()
        );
        let thread = DetTid::from_raw(42);
        let changed_owner = NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        assert!(
            receipts
                .dispatch(&mut backend, &prepare(4, changed_owner), &pins(), None)
                .is_err()
        );
        let mut replaced = pins();
        replaced[1].inode += 1;
        assert!(
            receipts
                .dispatch(&mut backend, &prepare(5, owner()), &replaced, None)
                .is_err()
        );
        assert_eq!(
            (
                backend.registrations,
                backend.preparations,
                backend.completions
            ),
            (1, 1, 1)
        );
        assert_eq!(receipts.0.get(&owner().thread).unwrap().identity, pins()[1]);
    }

    #[test]
    fn accepted_setter_dispatch_never_promotes_eexist_or_retries_unknown_registration() {
        let mut receipts = Registrations::default();
        let mut backend = Fake {
            registration_errno: Some(libc::EEXIST),
            ..Default::default()
        };
        for sequence in [1, 2] {
            let outcome = prepared(
                receipts
                    .dispatch(&mut backend, &prepare(sequence, owner()), &pins(), None)
                    .unwrap(),
            );
            assert_eq!(outcome.status.returned, -1);
            assert_eq!(outcome.status.errno, Some(libc::EEXIST));
            assert_eq!(outcome.raw, 0);
        }
        assert_eq!((backend.registrations, backend.preparations), (1, 0));
        let mut receipts = Registrations::default();
        let mut backend = Fake {
            registration_unknown: true,
            ..Default::default()
        };
        assert!(
            receipts
                .dispatch(&mut backend, &prepare(1, owner()), &pins(), None)
                .is_err()
        );
        backend.registration_unknown = false;
        assert!(
            receipts
                .dispatch(&mut backend, &prepare(2, owner()), &pins(), None)
                .is_err()
        );
        assert_eq!((backend.registrations, backend.preparations), (1, 0));
    }

    #[test]
    fn accepted_setter_dispatch_does_not_reset_unfinished_or_unknown_commands() {
        let mut receipts = Registrations::default();
        let mut backend = Fake::default();
        receipts
            .dispatch(&mut backend, &prepare(1, owner()), &pins(), None)
            .unwrap();
        assert!(
            receipts
                .dispatch(&mut backend, &prepare(2, owner()), &pins(), None)
                .is_err()
        );
        assert!(
            receipts
                .dispatch(&mut backend, &finish(3, 2, 1, owner()), &[], Some(&pins()))
                .is_err()
        );
        let mut changed = pins();
        changed[1].inode += 1;
        assert!(
            receipts
                .dispatch(&mut backend, &finish(4, 1, 1, owner()), &[], Some(&changed))
                .is_err()
        );
        backend.finish_unknown = true;
        assert!(
            receipts
                .dispatch(&mut backend, &finish(5, 1, 1, owner()), &[], Some(&pins()))
                .is_err()
        );
        assert!(
            receipts
                .dispatch(&mut backend, &prepare(6, owner()), &pins(), None)
                .is_err()
        );
        assert!(
            receipts
                .dispatch(&mut backend, &finish(7, 1, 1, owner()), &[], Some(&pins()))
                .is_err()
        );
        assert_eq!(
            (
                backend.registrations,
                backend.preparations,
                backend.completions
            ),
            (1, 1, 1)
        );
        let mut receipts = Registrations::default();
        let mut backend = Fake {
            preparation_unknown: true,
            ..Default::default()
        };
        assert!(
            receipts
                .dispatch(&mut backend, &prepare(1, owner()), &pins(), None)
                .is_err()
        );
        assert!(
            receipts
                .dispatch(&mut backend, &prepare(2, owner()), &pins(), None)
                .is_err()
        );
        assert_eq!((backend.registrations, backend.preparations), (1, 1));
    }

    #[derive(Default)]
    struct AcceptBackend {
        shared: Fake,
        unknown_collection: bool,
    }
    impl Backend for AcceptBackend {
        type Pin = PidfdIdentity;
        fn identity(&self, pin: &Self::Pin) -> io::Result<PidfdIdentity> {
            self.shared.identity(pin)
        }
        fn register(&mut self, pin: &Self::Pin) -> io::Result<CallStatus> {
            self.shared.register(pin)
        }
        fn prepare(&mut self, pin: &Self::Pin, setter: Setter) -> io::Result<Observation<u64>> {
            self.shared.prepare(pin, setter)
        }
        fn finish(
            &mut self,
            pin: &Self::Pin,
            command: u64,
        ) -> io::Result<Observation<CommandResult>> {
            self.shared.finish(pin, command)
        }
        fn prepare_accept(
            &mut self,
            _: &Self::Pin,
            accept: AcceptPreparation,
        ) -> io::Result<Observation<u64>> {
            assert_eq!(accept.lease, 17);
            assert_eq!(accept.mm, owner().mm.generation());
            assert_eq!((accept.fd, accept.flags), (6, libc::SOCK_CLOEXEC));
            self.shared.preparations += 1;
            Ok(Observation {
                status: status("prepare_accept"),
                raw: self.shared.preparations as u64,
            })
        }
        fn collect_accept(
            &mut self,
            _: &Self::Pin,
            command: u64,
        ) -> io::Result<Observation<super::super::AcceptedEffect>> {
            self.shared.completions += 1;
            if self.unknown_collection {
                return Err(io::Error::other("collection mutated before lost result"));
            }
            Ok(Observation {
                status: status("collect_accept"),
                raw: ffi::AcceptedEffect {
                    command: ffi::CommandResult {
                        command,
                        operation: 4,
                        returned: 8,
                        phase: 1,
                        ..Default::default()
                    },
                    installation: ffi::FdAccept {
                        command,
                        accept_lease: 17,
                        owner_mm: owner().mm.generation(),
                        returned_fd: 8,
                        phases: 127,
                        ..Default::default()
                    },
                }
                .into(),
            })
        }
    }
    fn prepare_accept(sequence: u64) -> Envelope {
        let mut e = prepare(sequence, owner());
        e.accept = Some(crate::network_replay::NetworkAcceptLeaseId(17));
        e.operation = Operation::PrepareAccept;
        e.body = serde_json::to_vec(&Request::PrepareAccept {
            identity: Identity {
                provider: 1,
                object: 9,
                namespace: 2,
            },
            lease: 17,
            mm: owner().mm.generation(),
            fd: 6,
            flags: libc::SOCK_CLOEXEC,
        })
        .unwrap();
        e
    }
    fn collect_accept(sequence: u64) -> Envelope {
        let mut e = finish(sequence, 1, 1, owner());
        e.operation = Operation::CollectAccept;
        e.accept = Some(crate::network_replay::NetworkAcceptLeaseId(17));
        e.body = serde_json::to_vec(&Request::CollectAccept {
            command: 1,
            prepared_request: 1,
        })
        .unwrap();
        e
    }
    #[test]
    fn accepted_effect_dispatch_uses_one_registration_and_excludes_setter_completion() {
        let mut registrations = Registrations::default();
        let mut backend = AcceptBackend::default();
        let ready = prepared(
            registrations
                .dispatch(&mut backend, &prepare_accept(1), &pins(), None)
                .unwrap(),
        );
        assert_eq!(ready.raw, 1);
        assert!(
            registrations
                .dispatch(&mut backend, &finish(2, 1, 1, owner()), &[], Some(&pins()))
                .is_err()
        );
        assert_eq!(backend.shared.completions, 0);
        let collected = registrations
            .dispatch(&mut backend, &collect_accept(3), &[], Some(&pins()))
            .unwrap();
        let Reply::AcceptedEffect(effect) = serde_json::from_slice(&collected).unwrap() else {
            panic!("wrong physical response")
        };
        assert_eq!(effect.raw.command.returned, 8);
        assert_eq!(effect.raw.installation.returned_fd, 8);
        let next = prepared(
            registrations
                .dispatch(&mut backend, &prepare(4, owner()), &pins(), None)
                .unwrap(),
        );
        assert_eq!(next.raw, 2);
        assert_eq!(
            (
                backend.shared.registrations,
                backend.shared.preparations,
                backend.shared.completions
            ),
            (1, 2, 1)
        );
    }
    #[test]
    fn accepted_effect_dispatch_unknown_collection_retains_the_original_command() {
        let mut registrations = Registrations::default();
        let mut backend = AcceptBackend {
            unknown_collection: true,
            ..Default::default()
        };
        registrations
            .dispatch(&mut backend, &prepare_accept(1), &pins(), None)
            .unwrap();
        assert!(
            registrations
                .dispatch(&mut backend, &collect_accept(2), &[], Some(&pins()))
                .is_err()
        );
        backend.unknown_collection = false;
        assert!(
            registrations
                .dispatch(&mut backend, &collect_accept(3), &[], Some(&pins()))
                .is_err()
        );
        assert!(
            registrations
                .dispatch(&mut backend, &prepare_accept(4), &pins(), None)
                .is_err()
        );
        assert_eq!(
            (
                backend.shared.registrations,
                backend.shared.preparations,
                backend.shared.completions
            ),
            (1, 1, 1)
        );
        assert_eq!(registrations.active_count(), 1);
    }

    #[test]
    fn original_dispatch_kind_mismatch_keeps_the_exact_preparation_live() {
        use crate::network_replay::original_connect::Kind;
        // Keep the old Connect fake and every old assertion unchanged. The
        // new Close preparation checks its distinct zero-address contract.
        struct OriginalBackend(Fake);
        impl Backend for OriginalBackend {
            type Pin = PidfdIdentity;
            fn identity(&self, p: &Self::Pin) -> io::Result<PidfdIdentity> {
                self.0.identity(p)
            }
            fn register(&mut self, p: &Self::Pin) -> io::Result<CallStatus> {
                self.0.register(p)
            }
            fn prepare(&mut self, p: &Self::Pin, s: Setter) -> io::Result<Observation<u64>> {
                self.0.prepare(p, s)
            }
            fn finish(&mut self, p: &Self::Pin, c: u64) -> io::Result<Observation<CommandResult>> {
                self.0.finish(p, c)
            }
            fn prepare_original(
                &mut self,
                p: &Self::Pin,
                r: OriginalPreparation,
            ) -> io::Result<Observation<u64>> {
                if r.kind == Kind::Connect {
                    return self.0.prepare_original(p, r);
                }
                assert_eq!(
                    (r.kind, r.call, r.mm, r.fd, r.address, r.length),
                    (Kind::Close, 17, owner().mm.generation(), 6, 0, 0)
                );
                self.0.preparations += 1;
                Ok(Observation {
                    status: status("prepare_original"),
                    raw: self.0.preparations as u64,
                })
            }
            fn cancel_original(&mut self, p: &Self::Pin, c: u64) -> io::Result<CallStatus> {
                self.0.cancel_original(p, c)
            }
        }
        for kind in [Kind::Connect, Kind::Close] {
            let mut registrations = Registrations::default();
            let mut backend = OriginalBackend(Fake::default());
            let pin = [pins()[1]];
            let mut prepare = prepare_original();
            let mut request: Request = serde_json::from_slice(&prepare.body).unwrap();
            if let Request::PrepareOriginalConnect {
                kind: actual,
                address,
                length,
                ..
            } = &mut request
            {
                *actual = kind;
                if kind == Kind::Close {
                    *address = 0;
                    *length = 0;
                }
            } else {
                unreachable!();
            }
            prepare.body = serde_json::to_vec(&request).unwrap();
            assert_eq!(
                prepared(
                    registrations
                        .dispatch(&mut backend, &prepare, &pin, None)
                        .unwrap()
                )
                .raw,
                1
            );
            let mut wrong = finish_original(false, 1);
            let mut request: Request = serde_json::from_slice(&wrong.body).unwrap();
            if let Request::CollectOriginalConnect { kind: actual, .. } = &mut request {
                *actual = if kind == Kind::Close {
                    Kind::Connect
                } else {
                    Kind::Close
                };
            } else {
                unreachable!();
            }
            wrong.body = serde_json::to_vec(&request).unwrap();
            assert!(
                registrations
                    .dispatch(&mut backend, &wrong, &[], Some(&pin))
                    .is_err()
            );
            assert_eq!(registrations.active_count(), 1);
            assert_eq!(
                (
                    backend.0.registrations,
                    backend.0.preparations,
                    backend.0.completions
                ),
                (1, 1, 0)
            );
            let response = registrations
                .dispatch(&mut backend, &finish_original(true, 1), &[], Some(&pin))
                .unwrap();
            assert!(matches!(
                serde_json::from_slice::<Reply>(&response).unwrap(),
                Reply::OriginalCanceled { command: 1, .. }
            ));
            assert_eq!(registrations.active_count(), 0);
            assert_eq!(backend.0.completions, 1);
        }
    }
}

#[cfg(test)]
mod table_enrollment_tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        registered: usize,
        prepared: usize,
        collected: usize,
        unknown: bool,
        native: i32,
    }
    fn status() -> CallStatus {
        CallStatus {
            operation: "table".into(),
            returned: 0,
            errno: None,
        }
    }
    impl Backend for Fake {
        type Pin = PidfdIdentity;
        fn identity(&self, p: &Self::Pin) -> io::Result<PidfdIdentity> {
            Ok(*p)
        }
        fn register(&mut self, _: &Self::Pin) -> io::Result<CallStatus> {
            self.registered += 1;
            Ok(status())
        }
        fn prepare(&mut self, _: &Self::Pin, _: Setter) -> io::Result<Observation<u64>> {
            panic!("not setter")
        }
        fn finish(&mut self, _: &Self::Pin, _: u64) -> io::Result<Observation<CommandResult>> {
            panic!("not setter")
        }
        fn prepare_table(
            &mut self,
            p: &Self::Pin,
            t: TablePreparation,
        ) -> io::Result<Observation<u64>> {
            assert_eq!(p.inode, 77);
            assert_eq!(t.registration, 9);
            assert_eq!(t.expected_table, 0);
            assert_eq!(t.mm, owner().mm.generation());
            self.prepared += 1;
            if self.unknown {
                return Err(io::Error::other("submitted then interrupted"));
            }
            Ok(Observation {
                status: status(),
                raw: 41,
            })
        }
        fn collect_table(
            &mut self,
            p: &Self::Pin,
            command: u64,
        ) -> io::Result<Observation<super::super::TableEnrollmentEffect>> {
            assert_eq!(p.inode, 77);
            assert_eq!(command, 41);
            self.collected += 1;
            Ok(Observation {
                status: status(),
                raw: ffi::TableEnrollmentEffect {
                    command: ffi::CommandResult {
                        command,
                        operation: 6,
                        task: (70071 << 32) | 70072,
                        start_boottime: 23,
                        returned: self.native,
                        ..Default::default()
                    },
                    enrollment: ffi::FdEnrollment {
                        command,
                        ptrace_return: self.native,
                        ..Default::default()
                    },
                }
                .into(),
            })
        }
    }
    fn owner() -> NetworkStreamOwner {
        let thread = DetTid::from_raw(71);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    fn pin() -> PidfdIdentity {
        PidfdIdentity {
            device: 1,
            inode: 77,
        }
    }
    fn prepare() -> Envelope {
        Envelope {
            run: [1; 16],
            sequence: 1,
            owner: Some(owner()),
            accept: None,
            operation: Operation::PrepareTableEnrollment,
            body: serde_json::to_vec(&Request::PrepareTableEnrollment {
                registration: 9,
                mm: owner().mm.generation(),
                expected_table: 0,
            })
            .unwrap(),
        }
    }
    fn finish() -> Envelope {
        Envelope {
            run: [1; 16],
            sequence: 2,
            owner: Some(owner()),
            accept: None,
            operation: Operation::CollectTableEnrollment,
            body: serde_json::to_vec(&Request::CollectTableEnrollment {
                command: 41,
                prepared_request: 1,
            })
            .unwrap(),
        }
    }
    #[test]
    fn exact_target_pidfd_only_and_native_error_is_not_relabelled() {
        let mut r = Registrations::default();
        let mut backend = Fake {
            native: -libc::EFAULT,
            ..Default::default()
        };
        for rights in [vec![], vec![pin(), pin()]] {
            assert!(r.dispatch(&mut backend, &prepare(), &rights, None).is_err())
        }
        assert_eq!(backend.registered, 0);
        let body = r
            .dispatch(&mut backend, &prepare(), &[pin()], None)
            .unwrap();
        assert!(matches!(
            serde_json::from_slice::<Reply>(&body).unwrap(),
            Reply::Prepared(Observation { raw: 41, .. })
        ));
        assert_eq!((backend.registered, backend.prepared), (1, 1));
        assert!(
            r.dispatch(
                &mut backend,
                &finish(),
                &[],
                Some(&[PidfdIdentity { inode: 78, ..pin() }])
            )
            .is_err()
        );
        assert_eq!(backend.collected, 0);
        for (command, prepared_request) in [(42, 1), (41, 3)] {
            let mut wrong = finish();
            wrong.body = serde_json::to_vec(&Request::CollectTableEnrollment {
                command,
                prepared_request,
            })
            .unwrap();
            assert!(
                r.dispatch(&mut backend, &wrong, &[], Some(&[pin()]))
                    .is_err()
            );
        }
        assert_eq!(backend.collected, 0);
        let body = r
            .dispatch(&mut backend, &finish(), &[], Some(&[pin()]))
            .unwrap();
        let Reply::TableEnrollmentEffect(out) = serde_json::from_slice(&body).unwrap() else {
            panic!("wrong operation")
        };
        assert_eq!(out.status.returned, 0);
        assert_ne!(out.raw.command.task >> 32, owner().thread.as_raw() as u64);
        assert_ne!(out.raw.command.task as u32, owner().thread.as_raw() as u32);
        assert_eq!(out.raw.command.task, (70071 << 32) | 70072);
        assert_eq!(out.raw.command.returned, -libc::EFAULT);
        assert_eq!(out.raw.enrollment.ptrace_return, -libc::EFAULT);
        assert_eq!(
            (backend.registered, backend.prepared, backend.collected),
            (1, 1, 1)
        );
        assert_eq!(r.active_count(), 0);
        assert!(
            r.dispatch(&mut backend, &finish(), &[], Some(&[pin()]))
                .is_err()
        );
        assert_eq!(backend.collected, 1);
    }
    #[test]
    fn malformed_or_canceled_enrollment_never_reopens_task_or_command() {
        let mut r = Registrations::default();
        let mut b = Fake::default();
        let mut wrong = prepare();
        wrong.accept = Some(crate::network_replay::NetworkAcceptLeaseId(1));
        assert!(r.dispatch(&mut b, &wrong, &[pin()], None).is_err());
        wrong = prepare();
        wrong.body = serde_json::to_vec(&Request::PrepareTableEnrollment {
            registration: 9,
            mm: owner().mm.generation() + 1,
            expected_table: 0,
        })
        .unwrap();
        assert!(r.dispatch(&mut b, &wrong, &[pin()], None).is_err());
        assert_eq!(b.registered, 0);
        b.unknown = true;
        assert!(r.dispatch(&mut b, &prepare(), &[pin()], None).is_err());
        assert_eq!(r.active_count(), 1);
        assert_eq!((b.registered, b.prepared), (1, 1));
        b.unknown = false;
        assert!(r.dispatch(&mut b, &prepare(), &[pin()], None).is_err());
        assert!(r.dispatch(&mut b, &finish(), &[], Some(&[pin()])).is_err());
        assert_eq!((b.registered, b.prepared, b.collected), (1, 1, 0));
    }
}

#[cfg(test)]
mod native_birth_tests {
    use super::*;
    #[derive(Default)]
    struct Native {
        arms: usize,
        observations: usize,
        cancels: usize,
        terminations: usize,
        collections: usize,
        dead: bool,
        returned: i32,
        unknown: bool,
    }
    fn status() -> CallStatus {
        CallStatus {
            operation: "native_birth".into(),
            returned: 0,
            errno: None,
        }
    }
    fn owner() -> NetworkStreamOwner {
        let thread = DetTid::from_raw(41);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    fn creator() -> PidfdIdentity {
        PidfdIdentity {
            device: 3,
            inode: 77,
        }
    }
    fn child() -> PidfdIdentity {
        PidfdIdentity {
            device: 3,
            inode: 91,
        }
    }
    impl Backend for Native {
        type Pin = PidfdIdentity;
        fn identity(&self, p: &Self::Pin) -> io::Result<PidfdIdentity> {
            Ok(*p)
        }
        fn register(&mut self, p: &Self::Pin) -> io::Result<CallStatus> {
            assert_eq!(*p, creator());
            Ok(status())
        }
        fn prepare(&mut self, _: &Self::Pin, _: Setter) -> io::Result<Observation<u64>> {
            panic!("not setter")
        }
        fn finish(&mut self, _: &Self::Pin, _: u64) -> io::Result<Observation<CommandResult>> {
            panic!("not setter")
        }
        fn terminate_birth(
            &mut self,
            p: &Self::Pin,
            command: u64,
        ) -> io::Result<Observation<super::super::NativeBirthTerminal>> {
            assert_eq!(*p, creator());
            assert_eq!(command, 7);
            self.terminations += 1;
            if self.unknown {
                return Err(io::Error::other("terminal effect outcome unknown"));
            }
            Ok(Observation {
                status: status(),
                raw: ffi::NativeBirthTerminal {
                    command: ffi::CommandResult {
                        command,
                        operation: 8,
                        phase: 3,
                        ..Default::default()
                    },
                    call: 17,
                    fd_call_present: 1,
                    task_absent: 1,
                    ..Default::default()
                }
                .into(),
            })
        }
        fn prepare_birth(
            &mut self,
            p: &Self::Pin,
            r: BirthPreparation,
        ) -> io::Result<Observation<u64>> {
            assert_eq!(*p, creator());
            assert_eq!(
                (r.call, r.mm, r.table, r.syscall),
                (17, owner().mm.generation(), 47, 435)
            );
            self.arms += 1;
            Ok(Observation {
                status: status(),
                raw: 7,
            })
        }
        fn observe_birth(
            &mut self,
            p: &Self::Pin,
            command: u64,
            terminal: bool,
        ) -> io::Result<Observation<super::super::NativeBirth>> {
            assert_eq!(*p, child());
            assert_eq!(command, 7);
            self.observations += 1;
            if self.unknown {
                return Err(io::Error::other(
                    "child marker operation completed but reply lost",
                ));
            }
            let raw = ffi::NativeBirth {
                command,
                call: 17,
                creator_task: (5001u64 << 32) | 5001,
                child_task: (5012u64 << 32) | 5012,
                ready: 1,
                exit_signal: if terminal { -1 } else { 17 },
                ..Default::default()
            }
            .into();
            Ok(Observation {
                status: status(),
                raw,
            })
        }
        fn collect_birth(
            &mut self,
            p: &Self::Pin,
            command: u64,
        ) -> io::Result<Observation<super::super::NativeBirthEffect>> {
            assert_eq!(*p, creator());
            assert_eq!(command, 7);
            assert!(self.dead);
            self.collections += 1;
            if self.unknown {
                return Err(io::Error::other(
                    "original Collect physical outcome unknown",
                ));
            }
            Ok(Observation {
                status: status(),
                raw: ffi::NativeBirthEffect {
                    command: ffi::CommandResult {
                        command,
                        operation: 8,
                        phase: 1,
                        returned: self.returned,
                        ..Default::default()
                    },
                    ..Default::default()
                }
                .into(),
            })
        }
        fn cancel_birth(&mut self, p: &Self::Pin, command: u64) -> io::Result<CallStatus> {
            assert_eq!(*p, creator());
            assert_eq!(command, 7);
            self.cancels += 1;
            Ok(status())
        }
    }
    fn request(sequence: u64, operation: Operation, request: Request) -> Envelope {
        Envelope {
            run: [3; 16],
            sequence,
            owner: Some(owner()),
            accept: None,
            operation,
            body: serde_json::to_vec(&request).unwrap(),
        }
    }
    fn prepare() -> Envelope {
        request(
            1,
            Operation::PrepareNativeBirth,
            Request::PrepareNativeBirth {
                call: 17,
                mm: owner().mm.generation(),
                table: 47,
                syscall: 435,
            },
        )
    }
    fn observe(sequence: u64, command: u64, terminal: bool) -> Envelope {
        request(
            sequence,
            Operation::ObserveNativeBirth,
            Request::ObserveNativeBirth {
                call: 17,
                command,
                prepared_request: 1,
                child: 42,
                terminal,
            },
        )
    }
    #[test]
    fn actual_birth_dispatch_binds_retained_creator_ticket_and_child_pin() {
        let mut r = Registrations::default();
        let mut n = Native::default();
        r.dispatch(&mut n, &prepare(), &[creator()], None).unwrap();
        assert!(
            r.dispatch(
                &mut n,
                &observe(2, 8, false),
                &[child()],
                Some(&[creator()])
            )
            .is_err()
        );
        assert!(
            r.dispatch(
                &mut n,
                &observe(3, 7, false),
                &[creator()],
                Some(&[creator()])
            )
            .is_err()
        );
        let wrong = PidfdIdentity {
            device: 3,
            inode: 78,
        };
        assert!(
            r.dispatch(&mut n, &observe(4, 7, false), &[child()], Some(&[wrong]))
                .is_err()
        );
        assert_eq!(n.observations, 0);
        let bytes = r
            .dispatch(
                &mut n,
                &observe(5, 7, false),
                &[child()],
                Some(&[creator()]),
            )
            .unwrap();
        let Reply::NativeBirth(result) = serde_json::from_slice(&bytes).unwrap() else {
            panic!("not birth")
        };
        assert_eq!(result.raw.command, 7);
        assert_ne!(result.raw.creator_task as i32, owner().thread.as_raw());
        assert!(
            r.dispatch(
                &mut n,
                &observe(6, 7, false),
                &[child()],
                Some(&[creator()])
            )
            .is_err()
        );
        assert_eq!((n.arms, n.observations), (1, 1));
        assert_eq!(r.active_count(), 1);
    }
    #[test]
    fn canceled_birth_observation_cannot_reexecute_or_be_replaced_by_terminal() {
        let mut r = Registrations::default();
        let mut n = Native {
            unknown: true,
            ..Default::default()
        };
        r.dispatch(&mut n, &prepare(), &[creator()], None).unwrap();
        assert!(
            r.dispatch(
                &mut n,
                &observe(2, 7, false),
                &[child()],
                Some(&[creator()])
            )
            .is_err()
        );
        n.unknown = false;
        assert!(
            r.dispatch(&mut n, &observe(3, 7, true), &[child()], Some(&[creator()]))
                .is_err()
        );
        assert!(r.dispatch(&mut n, &prepare(), &[creator()], None).is_err());
        assert_eq!((n.arms, n.observations), (1, 1));
        assert_eq!(r.active_count(), 1);
    }
    #[test]
    fn terminal_birth_and_known_uninvoked_cancellation_are_distinct_consumers() {
        let mut r = Registrations::default();
        let mut n = Native::default();
        r.dispatch(&mut n, &prepare(), &[creator()], None).unwrap();
        r.dispatch(&mut n, &observe(2, 7, true), &[child()], Some(&[creator()]))
            .unwrap();
        assert_eq!(n.observations, 1);
        assert_eq!(r.active_count(), 1);
        // A separate uninvoked command has no child observation at all.
        let mut r = Registrations::default();
        let mut n = Native::default();
        r.dispatch(&mut n, &prepare(), &[creator()], None).unwrap();
        let cancel = request(
            2,
            Operation::CancelNativeBirth,
            Request::CancelNativeBirth {
                call: 17,
                command: 7,
                prepared_request: 1,
            },
        );
        let bytes = r
            .dispatch(&mut n, &cancel, &[], Some(&[creator()]))
            .unwrap();
        assert!(
            matches!(serde_json::from_slice::<Reply>(&bytes).unwrap(),Reply::NativeBirthCanceled {command:7,status} if status.returned==0)
        );
        assert_eq!((n.arms, n.observations, n.cancels), (1, 0, 1));
        assert_eq!(r.active_count(), 0);
        assert!(
            r.dispatch(&mut n, &cancel, &[], Some(&[creator()]))
                .is_err()
        );
        assert_eq!(n.cancels, 1);
    }
    #[test]
    fn native_birth_syscall_shape_is_checked_before_arm_and_cannot_change_after_arm() {
        let mut r = Registrations::default();
        let mut n = Native::default();
        let wrong = request(
            1,
            Operation::PrepareNativeBirth,
            Request::PrepareNativeBirth {
                call: 17,
                mm: owner().mm.generation(),
                table: 47,
                syscall: 60,
            },
        );
        assert!(r.dispatch(&mut n, &wrong, &[creator()], None).is_err());
        assert_eq!(n.arms, 0);
        assert_eq!(r.active_count(), 0);
        r.dispatch(&mut n, &prepare(), &[creator()], None).unwrap();
        let changed = request(
            2,
            Operation::PrepareNativeBirth,
            Request::PrepareNativeBirth {
                call: 17,
                mm: owner().mm.generation(),
                table: 47,
                syscall: 56,
            },
        );
        assert!(r.dispatch(&mut n, &changed, &[creator()], None).is_err());
        assert_eq!(n.arms, 1);
        assert_eq!(r.active_count(), 1);
    }

    #[test]
    fn dead_birth_dispatch_uses_original_pin_and_never_retries_unknown_physical_retirement() {
        for unknown in [false, true] {
            let mut r = Registrations::default();
            let mut n = Native::default();
            r.dispatch(&mut n, &prepare(), &[creator()], None).unwrap();
            r.dispatch(
                &mut n,
                &observe(2, 7, false),
                &[child()],
                Some(&[creator()]),
            )
            .unwrap();
            let finish = request(
                3,
                Operation::TerminateNativeBirth,
                Request::TerminateNativeBirth {
                    call: 17,
                    command: 7,
                    prepared_request: 1,
                },
            );
            assert!(r.dispatch(&mut n, &finish, &[], Some(&[child()])).is_err());
            let changed = request(
                3,
                Operation::TerminateNativeBirth,
                Request::TerminateNativeBirth {
                    call: 17,
                    command: 8,
                    prepared_request: 1,
                },
            );
            assert!(
                r.dispatch(&mut n, &changed, &[], Some(&[creator()]))
                    .is_err()
            );
            assert_eq!(n.terminations, 0);
            assert_eq!(r.active_count(), 1);
            n.unknown = unknown;
            let result = r.dispatch(&mut n, &finish, &[], Some(&[creator()]));
            if unknown {
                assert!(result.is_err());
                assert_eq!(r.active_count(), 1);
            } else {
                assert!(
                    matches!(serde_json::from_slice::<Reply>(&result.unwrap()).unwrap(),
                    Reply::NativeBirthTerminated(value) if value.status.returned==0 && value.raw.command.phase==3)
                );
                assert_eq!(r.active_count(), 0);
            }
            n.unknown = false;
            assert!(
                r.dispatch(&mut n, &finish, &[], Some(&[creator()]))
                    .is_err()
            );
            assert_eq!(n.terminations, 1);
        }
    }

    #[test]
    fn queued_collect_dispatch_after_creator_death_keeps_same_pin_result_and_exclusive_finish() {
        for returned in [42, -libc::EINVAL] {
            for unknown in [false, true] {
                let mut r = Registrations::default();
                let mut n = Native::default();
                r.dispatch(&mut n, &prepare(), &[creator()], None).unwrap();
                // The existing transport owns this immutable Collect before the
                // creator dies. Dispatch must use its original retained PIDFD.
                let queued = request(
                    2,
                    Operation::CollectNativeBirth,
                    Request::CollectNativeBirth {
                        call: 17,
                        command: 7,
                        prepared_request: 1,
                    },
                );
                n.dead = true;
                n.returned = returned;
                n.unknown = unknown;
                assert!(r.dispatch(&mut n, &queued, &[], Some(&[child()])).is_err());
                assert_eq!(n.collections, 0);
                let reply = r.dispatch(&mut n, &queued, &[], Some(&[creator()]));
                if unknown {
                    assert!(reply.is_err());
                    assert_eq!(r.active_count(), 1);
                } else {
                    let Reply::NativeBirthEffect(value) =
                        serde_json::from_slice(&reply.unwrap()).unwrap()
                    else {
                        panic!("same Collect reply")
                    };
                    assert_eq!(
                        (value.raw.command.phase, value.raw.command.returned),
                        (1, returned)
                    );
                    assert_eq!(r.active_count(), 0);
                }
                n.unknown = false;
                assert!(
                    r.dispatch(&mut n, &queued, &[], Some(&[creator()]))
                        .is_err()
                );
                let terminal = request(
                    3,
                    Operation::TerminateNativeBirth,
                    Request::TerminateNativeBirth {
                        call: 17,
                        command: 7,
                        prepared_request: 1,
                    },
                );
                assert!(
                    r.dispatch(&mut n, &terminal, &[], Some(&[creator()]))
                        .is_err()
                );
                assert_eq!((n.arms, n.collections, n.terminations), (1, 1, 0));
            }
        }
    }
}

#[cfg(test)]
mod openat_auxiliary_tests {
    use super::*;
    fn owner() -> NetworkStreamOwner {
        let thread = DetTid::from_raw(31);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    fn ok(operation: &str) -> CallStatus {
        CallStatus {
            operation: operation.into(),
            returned: 0,
            errno: None,
        }
    }
    fn pin(inode: u64) -> PidfdIdentity {
        PidfdIdentity { device: 3, inode }
    }
    fn envelope(sequence: u64, operation: Operation, request: Request) -> Envelope {
        Envelope {
            run: [3; 16],
            sequence,
            owner: Some(owner()),
            accept: None,
            operation,
            body: serde_json::to_vec(&request).unwrap(),
        }
    }
    fn prepare(sequence: u64) -> Envelope {
        envelope(
            sequence,
            Operation::PrepareOriginalFileObservation,
            Request::PrepareOriginalFileObservation {
                call: 17,
                mm: owner().mm.generation(),
                fd: 88,
                role: AuxiliaryRole::File,
            },
        )
    }
    fn collect() -> Envelope {
        envelope(
            42,
            Operation::CollectOriginalFileObservation,
            Request::CollectOriginalFileObservation {
                call: 17,
                command: 91,
                prepared_request: 41,
                role: AuxiliaryRole::File,
            },
        )
    }
    fn retire() -> Envelope {
        envelope(
            43,
            Operation::RetireOriginalFileObservation,
            Request::RetireOriginalFileObservation {
                call: 17,
                prepared: 41,
                completed: 42,
            },
        )
    }
    fn original() -> super::super::OriginalEffect {
        let mut raw = ffi::OriginalEffect::default();
        raw.command.command = 71;
        raw.command.operation = 18;
        raw.original.selection.command = 71;
        raw.original.selection.call = 17;
        raw.original.selection.owner_mm = owner().mm.generation();
        raw.original.selection.provider = 7;
        raw.original.selection.file = 19;
        raw.original.returned = 17;
        raw.original.complete = 1;
        raw.into()
    }
    fn registry() -> Registrations {
        Registrations(BTreeMap::from([(
            owner().thread,
            Registration {
                owner: owner(),
                identity: pin(11),
                outcome: Some(ok("register original")),
                failure: None,
                active: None,
                last_allocator: Some(original()),
                auxiliary: None,
            },
        )]))
    }
    #[derive(Default)]
    struct Probe {
        calls: Vec<&'static str>,
        unknown_register: bool,
        failed_retire: bool,
        wrong_operation: bool,
        role: Option<AuxiliaryRole>,
        unknown_prepare: bool,
        unknown_collect: bool,
        wrong_file: bool,
    }
    impl Probe {
        fn selected(&self) -> ffi::OriginalSelection {
            let role = self.role.unwrap_or(AuxiliaryRole::File);
            let (address, flags, count) = role.operands();
            ffi::OriginalSelection {
                command: 91,
                call: 17,
                owner_mm: owner().mm.generation(),
                provider: 7,
                task: 41,
                task_start: 101,
                table: 13,
                file: if self.wrong_file { 20 } else { 19 },
                requested_fd: 88,
                user_address: address,
                address_length: flags,
                original_count: count,
                ready: 1,
                ..Default::default()
            }
        }
    }
    impl Backend for Probe {
        type Pin = PidfdIdentity;
        fn identity(&self, pin: &Self::Pin) -> io::Result<PidfdIdentity> {
            Ok(*pin)
        }
        fn register(&mut self, actual: &Self::Pin) -> io::Result<CallStatus> {
            assert_eq!(*actual, pin(22));
            self.calls.push("register");
            if self.unknown_register {
                Err(io::Error::other("registered then lost reply"))
            } else {
                Ok(ok("register auxiliary"))
            }
        }
        fn prepare(&mut self, _: &Self::Pin, _: Setter) -> io::Result<Observation<u64>> {
            unreachable!()
        }
        fn finish(&mut self, _: &Self::Pin, _: u64) -> io::Result<Observation<CommandResult>> {
            unreachable!()
        }
        fn prepare_auxiliary_file(
            &mut self,
            actual: &Self::Pin,
            call: u64,
            mm: u64,
            fd: i32,
        ) -> io::Result<Observation<u64>> {
            assert_eq!(*actual, pin(22));
            assert_eq!((call, mm, fd), (17, owner().mm.generation(), 88));
            self.calls.push("prepare F_GETFL");
            Ok(Observation {
                status: ok("ap_prepare_auxiliary_file"),
                raw: 91,
            })
        }
        fn prepare_helper_receive(
            &mut self,
            actual: &Self::Pin,
            call: u64,
            mm: u64,
            fd: i32,
            role: AuxiliaryRole,
        ) -> io::Result<Observation<u64>> {
            assert_eq!(
                (*actual, call, mm, fd),
                (pin(22), 17, owner().mm.generation(), 88)
            );
            assert!(role.is_receive() && role.valid());
            self.role = Some(role);
            self.calls.push("prepare receive");
            if self.unknown_prepare {
                return Err(io::Error::other("armed then lost reply"));
            }
            Ok(Observation {
                status: ok(role.prepare_name()),
                raw: 91,
            })
        }
        fn read_original(
            &mut self,
            actual: &Self::Pin,
            command: u64,
        ) -> io::Result<Observation<super::super::OriginalSelection>> {
            assert_eq!((*actual, command), (pin(22), 91));
            self.calls.push("select");
            Ok(Observation {
                status: ok("select"),
                raw: self.selected().into(),
            })
        }
        fn collect_original(
            &mut self,
            actual: &Self::Pin,
            command: u64,
        ) -> io::Result<Observation<super::super::OriginalEffect>> {
            assert_eq!((*actual, command), (pin(22), 91));
            self.calls.push("collect");
            if self.unknown_collect {
                return Err(io::Error::other("actual collect then lost reply"));
            }
            let mut raw = ffi::OriginalEffect::default();
            raw.command.command = 91;
            raw.command.operation = if self.wrong_operation {
                10
            } else {
                self.role.unwrap_or(AuxiliaryRole::File).operation()
            };
            raw.original.selection = self.selected();
            raw.original.returned = libc::O_RDONLY;
            raw.original.complete = 1;
            Ok(Observation {
                status: ok("collect"),
                raw: raw.into(),
            })
        }
        fn retire_auxiliary(&mut self, actual: &Self::Pin) -> io::Result<CallStatus> {
            assert_eq!(*actual, pin(22));
            self.calls.push("retire");
            Ok(if self.failed_retire {
                CallStatus {
                    operation: "retire".into(),
                    returned: -1,
                    errno: Some(libc::EIO),
                }
            } else {
                ok("retire")
            })
        }
    }
    fn receive_prepare(role: AuxiliaryRole) -> Envelope {
        envelope(
            41,
            Operation::PrepareOriginalFileObservation,
            Request::PrepareOriginalFileObservation {
                call: 17,
                mm: owner().mm.generation(),
                fd: 88,
                role,
            },
        )
    }
    fn receive_collect(role: AuxiliaryRole) -> Envelope {
        envelope(
            42,
            Operation::CollectOriginalFileObservation,
            Request::CollectOriginalFileObservation {
                call: 17,
                command: 91,
                prepared_request: 41,
                role,
            },
        )
    }
    fn role(kind: ReceiveKind) -> AuxiliaryRole {
        AuxiliaryRole::Receive {
            kind,
            address: 0x8000,
            count: if kind == ReceiveKind::Drain { 1 } else { 1024 },
            provider: 7,
            file: 19,
        }
    }
    #[test]
    fn helper_receive_dispatch_preserves_guest_registration_and_refuses_role_or_worker_substitution()
     {
        for kind in [ReceiveKind::Drain, ReceiveKind::Peek] {
            let role = role(kind);
            let mut registry = registry();
            let mut backend = Probe::default();
            // Stream Call ownership does not require an unrelated Openat command.
            registry.0.get_mut(&owner().thread).unwrap().last_allocator = None;
            let worker = [pin(22)];
            assert!(
                registry
                    .dispatch(&mut backend, &receive_prepare(role), &[pin(11)], None)
                    .is_err()
            );
            assert!(backend.calls.is_empty());
            registry
                .dispatch(&mut backend, &receive_prepare(role), &worker, None)
                .unwrap();
            assert!(
                registry
                    .dispatch(&mut backend, &collect(), &[], Some(&worker))
                    .is_err()
            );
            assert!(
                registry
                    .dispatch(&mut backend, &receive_collect(role), &[], Some(&[pin(33)]))
                    .is_err()
            );
            assert_eq!(backend.calls, ["register", "prepare receive"]);
            let reply: Reply = serde_json::from_slice(
                &registry
                    .dispatch(&mut backend, &receive_collect(role), &[], Some(&worker))
                    .unwrap(),
            )
            .unwrap();
            let Reply::OriginalFileObservation {
                effect: Some(effect),
                ..
            } = reply
            else {
                panic!("typed helper result");
            };
            assert_eq!(effect.raw.command.operation, role.operation());
            registry
                .dispatch(&mut backend, &retire(), &[], Some(&worker))
                .unwrap();
            assert_eq!(registry.active_count(), 0);
            assert_eq!(registry.0[&owner().thread].identity, pin(11));
            assert_eq!(
                backend.calls,
                ["register", "prepare receive", "select", "collect", "retire"]
            );
        }
    }
    #[test]
    fn helper_receive_unknown_native_outcomes_latch_without_rearming_or_recollecting() {
        for kind in [ReceiveKind::Drain, ReceiveKind::Peek] {
            for stage in 0..4 {
                let role = role(kind);
                let mut registry = registry();
                let mut backend = Probe {
                    unknown_register: stage == 0,
                    unknown_prepare: stage == 1,
                    unknown_collect: stage == 2,
                    failed_retire: stage == 3,
                    ..Default::default()
                };
                let worker = [pin(22)];
                let prepare =
                    registry.dispatch(&mut backend, &receive_prepare(role), &worker, None);
                if stage <= 1 {
                    assert!(prepare.is_err());
                } else {
                    prepare.unwrap();
                    let collect =
                        registry.dispatch(&mut backend, &receive_collect(role), &[], Some(&worker));
                    if stage == 2 {
                        assert!(collect.is_err());
                    } else {
                        collect.unwrap();
                        let result: Reply = serde_json::from_slice(
                            &registry
                                .dispatch(&mut backend, &retire(), &[], Some(&worker))
                                .unwrap(),
                        )
                        .unwrap();
                        assert!(matches!(
                            result,
                            Reply::OriginalFileObservationRetired(CallStatus { returned: -1, .. })
                        ));
                    }
                }
                let held = registry.0[&owner().thread].auxiliary.as_ref().unwrap();
                assert_eq!(
                    (held.role, held.identity, held.call, held.request),
                    (role, pin(22), 17, 41)
                );
                assert_eq!(registry.active_count(), 1);
                let calls = backend.calls.clone();
                assert!(
                    registry
                        .dispatch(&mut backend, &receive_prepare(role), &worker, None)
                        .is_err()
                );
                assert!(
                    registry
                        .dispatch(&mut backend, &receive_collect(role), &[], Some(&worker))
                        .is_err()
                );
                assert!(
                    registry
                        .dispatch(&mut backend, &retire(), &[], Some(&worker))
                        .is_err()
                );
                assert_eq!(backend.calls, calls);
            }
        }
    }
    #[test]
    fn helper_receive_wrong_held_file_refuses_before_native_collection_and_keeps_owner() {
        let mut registry = registry();
        let mut backend = Probe {
            wrong_file: true,
            ..Default::default()
        };
        let role = role(ReceiveKind::Drain);
        let worker = [pin(22)];
        registry
            .dispatch(&mut backend, &receive_prepare(role), &worker, None)
            .unwrap();
        assert!(
            registry
                .dispatch(&mut backend, &receive_collect(role), &[], Some(&worker))
                .is_err()
        );
        assert!(
            registry
                .dispatch(&mut backend, &receive_collect(role), &[], Some(&worker))
                .is_err()
        );
        assert_eq!(backend.calls, ["register", "prepare receive", "select"]);
        assert_eq!(registry.active_count(), 1);
    }
    #[test]
    fn original_openat_auxiliary_refuses_guest_role_completion_without_retrying() {
        let mut registry = registry();
        let mut backend = Probe {
            wrong_operation: true,
            ..Default::default()
        };
        let worker = [pin(22)];
        registry
            .dispatch(&mut backend, &prepare(41), &worker, None)
            .unwrap();
        assert!(
            registry
                .dispatch(&mut backend, &collect(), &[], Some(&worker))
                .is_err()
        );
        let retained = registry.0[&owner().thread].auxiliary.as_ref().unwrap();
        assert_eq!(
            (
                retained.identity,
                retained.call,
                retained.command,
                retained.completion
            ),
            (pin(22), 17, Some(91), Some(42))
        );
        assert_eq!(registry.active_count(), 1);
        assert!(
            registry
                .dispatch(&mut backend, &collect(), &[], Some(&worker))
                .is_err()
        );
        assert_eq!(
            backend.calls,
            ["register", "prepare F_GETFL", "select", "collect"]
        );
    }
    #[test]
    fn original_openat_auxiliary_dispatch_joins_exact_worker_and_retires_once() {
        let mut registry = registry();
        let mut backend = Probe::default();
        let worker = [pin(22)];
        // A different pinned PIDFD identity cannot replace this task. This
        // mock checks identity matching; it does not model native task death.
        assert!(
            registry
                .dispatch(&mut backend, &prepare(41), &[pin(11)], None)
                .is_err()
        );
        assert!(backend.calls.is_empty());
        let prepared: Reply = serde_json::from_slice(
            &registry
                .dispatch(&mut backend, &prepare(41), &worker, None)
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            prepared,
            Reply::Prepared(Observation { raw: 91, .. })
        ));
        assert_eq!(registry.active_count(), 1);
        assert!(
            registry
                .dispatch(&mut backend, &collect(), &[], Some(&[pin(33)]))
                .is_err()
        );
        assert_eq!(backend.calls, ["register", "prepare F_GETFL"]);
        let completion: Reply = serde_json::from_slice(
            &registry
                .dispatch(&mut backend, &collect(), &[], Some(&worker))
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            completion,
            Reply::OriginalFileObservation {
                effect: Some(_),
                ..
            }
        ));
        assert!(
            registry
                .dispatch(&mut backend, &collect(), &[], Some(&worker))
                .is_err()
        );
        let retired: Reply = serde_json::from_slice(
            &registry
                .dispatch(&mut backend, &retire(), &[], Some(&worker))
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            retired,
            Reply::OriginalFileObservationRetired(CallStatus {
                returned: 0,
                errno: None,
                ..
            })
        ));
        assert_eq!(registry.active_count(), 0);
        assert!(
            registry
                .dispatch(&mut backend, &retire(), &[], Some(&worker))
                .is_err()
        );
        assert_eq!(
            backend.calls,
            ["register", "prepare F_GETFL", "select", "collect", "retire"]
        );
        assert_eq!(registry.0[&owner().thread].identity, pin(11));
        assert_eq!(
            registry.0[&owner().thread]
                .last_allocator
                .as_ref()
                .unwrap()
                .original
                .selection
                .file,
            19
        );
    }
    #[test]
    fn original_openat_auxiliary_unknown_outcomes_keep_the_same_owner_without_retry() {
        for registration in [true, false] {
            let mut registry = registry();
            let mut backend = Probe {
                unknown_register: registration,
                failed_retire: !registration,
                ..Default::default()
            };
            let worker = [pin(22)];
            let prepared = registry.dispatch(&mut backend, &prepare(41), &worker, None);
            if registration {
                assert!(prepared.is_err());
            } else {
                prepared.unwrap();
                registry
                    .dispatch(&mut backend, &collect(), &[], Some(&worker))
                    .unwrap();
                let reply: Reply = serde_json::from_slice(
                    &registry
                        .dispatch(&mut backend, &retire(), &[], Some(&worker))
                        .unwrap(),
                )
                .unwrap();
                assert!(matches!(
                    reply,
                    Reply::OriginalFileObservationRetired(CallStatus {
                        returned: -1,
                        errno: Some(libc::EIO),
                        ..
                    })
                ));
                assert!(
                    registry
                        .dispatch(&mut backend, &retire(), &[], Some(&worker))
                        .is_err()
                );
            }
            assert_eq!(registry.active_count(), 1);
            let before = backend.calls.clone();
            assert!(
                registry
                    .dispatch(&mut backend, &prepare(44), &worker, None)
                    .is_err()
            );
            assert_eq!(backend.calls, before);
            let held = registry.0[&owner().thread].auxiliary.as_ref().unwrap();
            assert_eq!((held.identity, held.call, held.request), (pin(22), 17, 41));
        }
    }
}

#[cfg(test)]
mod control_selection_tests {
    use super::*;
    use crate::network_replay::original_connect::Kind;
    fn owner() -> NetworkStreamOwner {
        let thread = DetTid::from_raw(41);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    fn pin(inode: u64) -> PidfdIdentity {
        PidfdIdentity { device: 3, inode }
    }
    fn ok(operation: &str) -> CallStatus {
        CallStatus {
            operation: operation.into(),
            returned: 0,
            errno: None,
        }
    }
    fn registry(kind: Kind) -> Registrations {
        Registrations(BTreeMap::from([(
            owner().thread,
            Registration {
                owner: owner(),
                identity: pin(11),
                outcome: Some(ok("register")),
                failure: None,
                last_allocator: None,
                auxiliary: None,
                active: Some(Active {
                    original_kind: Some(kind),
                    original_call: Some(17),
                    operation: Operation::PrepareOriginalConnect,
                    request: 41,
                    command: Some(91),
                    finish_submitted: false,
                    failed_finish: None,
                    birth_observation_submitted: false,
                }),
            },
        )]))
    }
    fn request() -> Envelope {
        Envelope {
            run: [3; 16],
            sequence: 42,
            owner: Some(owner()),
            accept: None,
            operation: Operation::AwaitOriginalSelection,
            body: serde_json::to_vec(&Request::AwaitOriginalSelection {
                call: 17,
                command: 91,
                prepared_request: 41,
            })
            .unwrap(),
        }
    }
    #[derive(Default)]
    struct Probe {
        single: usize,
        pair: usize,
        errno: Option<i32>,
        wrong_call: bool,
    }
    impl Backend for Probe {
        type Pin = PidfdIdentity;
        fn identity(&self, p: &Self::Pin) -> io::Result<PidfdIdentity> {
            Ok(*p)
        }
        fn register(&mut self, _: &Self::Pin) -> io::Result<CallStatus> {
            unreachable!()
        }
        fn prepare(&mut self, _: &Self::Pin, _: Setter) -> io::Result<Observation<u64>> {
            unreachable!()
        }
        fn finish(&mut self, _: &Self::Pin, _: u64) -> io::Result<Observation<CommandResult>> {
            unreachable!()
        }
        fn read_original(
            &mut self,
            p: &Self::Pin,
            command: u64,
        ) -> io::Result<Observation<super::super::OriginalSelection>> {
            assert_eq!((*p, command), (pin(11), 91));
            self.single += 1;
            Ok(Observation {
                status: ok("ap_read_original_selection"),
                raw: ffi::OriginalSelection {
                    command: 91,
                    call: 17,
                    ..Default::default()
                }
                .into(),
            })
        }
        fn read_original_control(
            &mut self,
            p: &Self::Pin,
            command: u64,
        ) -> io::Result<Observation<super::super::OriginalResult>> {
            assert_eq!((*p, command), (pin(11), 91));
            self.pair += 1;
            let mut raw = ffi::OriginalResult::default();
            raw.selection.command = 91;
            raw.selection.call = if self.wrong_call { 18 } else { 17 };
            let status = match self.errno {
                Some(errno) => CallStatus {
                    operation: "ap_read_original_epoll_ctl_selection".into(),
                    returned: -1,
                    errno: Some(errno),
                },
                None => ok("ap_read_original_epoll_ctl_selection"),
            };
            Ok(Observation {
                status,
                raw: raw.into(),
            })
        }
    }
    #[test]
    fn original_control_selection_dispatch_uses_retained_kind_and_exact_original_pin() {
        for kind in [Kind::Connect, Kind::EpollCtl] {
            let mut r = registry(kind);
            let mut b = Probe::default();
            let reply: Reply = serde_json::from_slice(
                &r.dispatch(&mut b, &request(), &[], Some(&[pin(11)]))
                    .unwrap(),
            )
            .unwrap();
            if kind == Kind::EpollCtl {
                assert!(matches!(reply, Reply::OriginalControlSelection(_)));
                assert_eq!((b.single, b.pair), (0, 1));
            } else {
                assert!(matches!(reply, Reply::OriginalSelection(_)));
                assert_eq!((b.single, b.pair), (1, 0));
            }
            assert_eq!(r.active_count(), 1);
            assert_eq!(r.0[&owner().thread].identity, pin(11));
        }
        for wrong in 0..7 {
            let mut r = registry(Kind::EpollCtl);
            let mut b = Probe::default();
            let mut q = request();
            let mut pins = [pin(11)];
            let mut rights = Vec::new();
            match wrong {
                0 => pins[0] = pin(12),
                1 => q.owner = None,
                2 => {
                    q.body = serde_json::to_vec(&Request::AwaitOriginalSelection {
                        call: 18,
                        command: 91,
                        prepared_request: 41,
                    })
                    .unwrap()
                }
                3 => {
                    q.body = serde_json::to_vec(&Request::AwaitOriginalSelection {
                        call: 17,
                        command: 92,
                        prepared_request: 41,
                    })
                    .unwrap()
                }
                4 => {
                    q.body = serde_json::to_vec(&Request::AwaitOriginalSelection {
                        call: 17,
                        command: 91,
                        prepared_request: 40,
                    })
                    .unwrap()
                }
                5 => rights.push(pin(11)),
                6 => {
                    r.0.get_mut(&owner().thread)
                        .unwrap()
                        .active
                        .as_mut()
                        .unwrap()
                        .finish_submitted = true
                }
                _ => unreachable!(),
            }
            assert!(
                r.dispatch(&mut b, &q, &rights, Some(&pins)).is_err(),
                "mutation {wrong}"
            );
            assert_eq!((b.single, b.pair), (0, 0));
            assert_eq!(r.active_count(), 1);
        }
    }
    #[test]
    fn original_control_selection_pending_and_identity_failure_keep_original_owner() {
        for errno in [libc::ENODATA, libc::ENOENT, libc::EIO] {
            let mut r = registry(Kind::EpollCtl);
            let mut b = Probe {
                errno: Some(errno),
                ..Default::default()
            };
            let reply: Reply = serde_json::from_slice(
                &r.dispatch(&mut b, &request(), &[], Some(&[pin(11)]))
                    .unwrap(),
            )
            .unwrap();
            let Reply::OriginalControlSelection(observed) = reply else {
                panic!("pair degraded")
            };
            assert_eq!(observed.status.errno, Some(errno));
            assert_eq!(observed.status.returned, -1);
            assert_eq!(r.active_count(), 1);
            assert_eq!(
                r.0[&owner().thread].active.as_ref().unwrap().command,
                Some(91)
            );
            assert_eq!((b.single, b.pair), (0, 1));
        }
        let mut r = registry(Kind::EpollCtl);
        let mut b = Probe {
            wrong_call: true,
            ..Default::default()
        };
        assert!(
            r.dispatch(&mut b, &request(), &[], Some(&[pin(11)]))
                .is_err()
        );
        assert_eq!(r.active_count(), 1);
        assert_eq!(
            r.0[&owner().thread].active.as_ref().unwrap().original_call,
            Some(17)
        );
    }
}
