//! Provider registration receipts derived from authenticated controller transfers.
//! This does not register semantic task or FD ownership. The transport inbox
//! retains each pidfd, so a pidfs identity remains pinned for this receipt's life.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;

use super::CallStatus;
use super::CommandResult;
use super::Envelope;
use super::Identity;
use super::NetworkStreamOwner;
use super::Observation;
use super::Operation;
use super::Reply;
use super::Request;
use super::ffi;
use crate::types::DetTid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PidfdIdentity {
    device: u64,
    inode: u64,
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
enum Preparation {
    Setter(Setter),
    Accept(AcceptPreparation),
}

trait Backend {
    type Pin;
    fn identity(&self, pidfd: &Self::Pin) -> io::Result<PidfdIdentity>;
    fn register(&mut self, pidfd: &Self::Pin) -> io::Result<CallStatus>;
    fn prepare(&mut self, pidfd: &Self::Pin, setter: Setter) -> io::Result<Observation<u64>>;
    fn finish(&mut self, pidfd: &Self::Pin, command: u64)
    -> io::Result<Observation<CommandResult>>;
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
    fn identity(&self, pidfd: &OwnedFd) -> io::Result<PidfdIdentity> {
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
    operation: Operation,
    request: u64,
    command: Option<u64>,
    finish_submitted: bool,
}
struct Registration {
    owner: NetworkStreamOwner,
    identity: PidfdIdentity,
    outcome: Option<CallStatus>,
    failure: Option<String>,
    active: Option<Active>,
}

#[derive(Default)]
pub(super) struct Registrations(BTreeMap<DetTid, Registration>);
impl Registrations {
    pub(super) fn active_count(&self) -> usize {
        self.0
            .values()
            .filter(|entry| entry.active.is_some())
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
            (operation @ (Operation::PrepareSetter | Operation::PrepareAccept), request)
                if rights.len() == 2 =>
            {
                let preparation = match (operation, request) {
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
                let pidfd = &rights[1];
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
                    if receipt.active.is_some() {
                        return Err(io::Error::other(
                            "provider setter still has an unresolved command",
                        ));
                    }
                    receipt.active = Some(Active {
                        operation,
                        request: envelope.sequence,
                        command: None,
                        finish_submitted: false,
                    });
                    let outcome = match preparation {
                        Preparation::Setter(setter) => backend.prepare(pidfd, setter)?,
                        Preparation::Accept(accept) => backend.prepare_accept(pidfd, accept)?,
                    };
                    if outcome.status.returned == 0 && outcome.raw != 0 {
                        receipt.active.as_mut().unwrap().command = Some(outcome.raw);
                    }
                    Reply::Prepared(outcome)
                }
            }
            (operation @ (Operation::FinishSetter | Operation::CollectAccept), request)
                if rights.is_empty() =>
            {
                let (command, prepared_request, preparation) = match (operation, request) {
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
                let pins = prepared_rights
                    .filter(|pins| pins.len() == 2)
                    .ok_or_else(|| io::Error::other("setter finish lacks retained preparation"))?;
                let identity = backend.identity(&pins[1])?;
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
                    })
                    .ok_or_else(|| io::Error::other("setter finish changed active command"))?;
                if active.finish_submitted {
                    return Err(io::Error::other(
                        "setter finish remains submitted; reuse its retained transport reply",
                    ));
                }
                active.finish_submitted = true;
                let reply = match operation {
                    Operation::FinishSetter => Reply::Command(backend.finish(&pins[1], command)?),
                    Operation::CollectAccept => {
                        Reply::AcceptedEffect(backend.collect_accept(&pins[1], command)?)
                    }
                    _ => unreachable!(),
                };
                let (status, returned_command) = match &reply {
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
                    receipt.active = None;
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
}
