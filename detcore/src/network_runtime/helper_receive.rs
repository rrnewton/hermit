//! Authenticated helper receives on the existing stream Call and blocking worker.
//! The provider's copy records, not a scratch reread, supply payload bytes.
//! Copy order does not establish a guest release or virtual-time cut.
use std::io;
use std::sync::Arc;
use std::sync::OnceLock;

use super::accepted_controller::Controller;
use super::accepted_controller::Effect as ControllerEffect;
use super::accepted_provider::AuxiliaryRole;
use super::accepted_provider::CallStatus;
use super::accepted_provider::Observation;
use super::accepted_provider::OriginalEffect;
use super::accepted_provider::OriginalSelection;
use super::accepted_provider::PidfdIdentity;
use super::accepted_provider::ReceiveKind;
use super::accepted_provider::Reply;
use super::accepted_provider::Request;
use super::native_peer::Execution;
use super::native_peer::Observation as NativeObservation;
use super::original_installation::FileIdentity;
use super::original_read_copy::Capture;
use super::original_read_copy::ReadCopyCustody;
use super::*;
use crate::network_replay::NetworkStreamCallId;
use crate::network_replay::NetworkStreamLeaseId;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::NetworkStreamPhysicalEffect as Effect;
use crate::network_replay::NetworkStreamPhysicalResult as ResultValue;

/// Issued only from the existing Pending's held-file capture. These immutable
/// fields never cross serde; the same Arc is retained by the engine before any
/// helper preparation or native effect. The custody contains no engine handle.
#[derive(Debug)]
pub(crate) struct Binding {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    lease: NetworkStreamLeaseId,
    identity: FileIdentity,
    effect: Effect,
    custody: Arc<ReadCopyCustody>,
    // Actual worker join, issued after the exact retained result passes preflight.
    // This receipt neither excludes a later worker nor permits guest stores.
    joined: OnceLock<JoinedNativeWorkerReceipt>,
    attempts: OnceLock<Vec<super::original_read_copy::NativeAttempt>>,
    // Present only after the actual confirmed probe lease was transferred to
    // this delivery on the same held file. Not numeric correlation or serde.
    predecessor: Option<Completion>,
}
impl PartialEq for Binding {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for Binding {}
impl Binding {
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.owner
    }
    pub(crate) fn call(&self) -> NetworkStreamCallId {
        self.call
    }
    pub(crate) fn lease(&self) -> NetworkStreamLeaseId {
        self.lease
    }
    pub(crate) fn effect(&self) -> &Effect {
        &self.effect
    }
    pub(super) fn matches_file(&self, identity: FileIdentity) -> bool {
        self.identity == identity
    }
    pub(crate) fn succeeds(&self, source: &Completion) -> bool {
        self.predecessor.as_ref() == Some(source)
            && self.identity == source.binding.identity
            && self.owner == source.binding.owner
            && self.call == source.binding.call
            && self.lease != source.binding.lease
            && matches!(self.effect, Effect::Drain { .. })
    }
    pub(crate) fn owns_attempt(&self, attempt: &super::original_read_copy::NativeAttempt) -> bool {
        attempt.belongs_to(self.owner, self.call, &self.custody)
            && self
                .identity
                .matches(attempt.selection().provider, attempt.selection().file)
            && match self.effect {
                Effect::Drain { .. } => attempt.operation() == 21,
                Effect::Peek { .. } => attempt.operation() == 22,
                _ => false,
            }
    }
}

/// Controller-local proof of the exact collected Arc, not serializable metadata.
/// Eq tests identity of both owners, never structural equality of native bytes.
#[derive(Clone, Debug)]
pub(crate) struct Completion {
    binding: Arc<Binding>,
    capture: Arc<Capture>,
}
impl PartialEq for Completion {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.binding, &other.binding) && Arc::ptr_eq(&self.capture, &other.capture)
    }
}
impl Eq for Completion {}
impl Completion {
    pub(crate) fn binding(&self) -> &Arc<Binding> {
        &self.binding
    }
    pub(crate) fn capture(&self) -> &Arc<Capture> {
        &self.capture
    }
    pub(crate) fn attempts(&self) -> &[super::original_read_copy::NativeAttempt] {
        self.binding
            .attempts
            .get()
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
    pub(crate) fn joined_worker(&self) -> io::Result<&JoinedNativeWorkerReceipt> {
        self.binding
            .joined
            .get()
            .ok_or_else(|| io::Error::other("helper worker has not actually joined"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CopyEnvelope {
    sequence: u64,
    prepared: u64,
    first: u64,
    records: usize,
    end: Option<super::original_read_copy::End>,
}

const PUBLICATION_UNIT: usize = 1024;
const DRAIN_VIEW: usize = 512;
const MAX_RW_COUNT: usize = 0x7fff_f000;

/// Immutable value snapshot retained by failed-run terminal evidence. The live
/// Pending separately owns the actual task and scratch until it is settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Receipt {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    identity: FileIdentity,
    effect: Effect,
    task: Option<PidfdIdentity>,
    role: Option<AuxiliaryRole>,
    prepare_request: Option<u64>,
    prepared: Option<Observation<u64>>,
    native: Option<CallStatus>,
    complete_request: Option<u64>,
    completed: Option<(
        Observation<OriginalSelection>,
        Option<Observation<OriginalEffect>>,
    )>,
    copy_requests: Vec<(u64, u64)>,
    copies: Vec<CopyEnvelope>,
    binding: Arc<Binding>,
    retirement_request: Option<u64>,
    retired: Option<CallStatus>,
    transport_retired: bool,
    error: Option<String>,
}

#[derive(Debug)]
struct State {
    receipt: Receipt,
    task: Option<Arc<OwnedFd>>,
    scratch: Option<Arc<Mutex<Scratch>>>,
}

/// Clone only custody of the same Pending. This is not another Call registry.
#[derive(Clone, Debug)]
pub(super) struct Held {
    binding: Arc<Binding>,
    state: Arc<Mutex<State>>,
}
impl Held {
    pub(super) fn new_bound(
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        identity: FileIdentity,
        effect: Effect,
    ) -> io::Result<Self> {
        Self::new_successor(owner, call, lease, identity, effect, None)
    }
    pub(super) fn new_successor(
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        identity: FileIdentity,
        effect: Effect,
        predecessor: Option<Completion>,
    ) -> io::Result<Self> {
        match effect {
            Effect::Drain { maximum } if (1..=DRAIN_VIEW).contains(&maximum) => {}
            Effect::Peek { maximum } if (PUBLICATION_UNIT..=MAX_RW_COUNT).contains(&maximum) => {}
            _ => {
                return Err(io::Error::other(
                    "helper receive changed existing bounded effect",
                ));
            }
        }
        let binding = Arc::new(Binding {
            owner,
            call,
            lease,
            identity,
            effect: effect.clone(),
            custody: Arc::new(ReadCopyCustody::default()),
            joined: OnceLock::new(),
            attempts: OnceLock::new(),
            predecessor,
        });
        Ok(Self {
            binding: binding.clone(),
            state: Arc::new(Mutex::new(State {
                receipt: Receipt {
                    owner,
                    call,
                    identity,
                    effect,
                    task: None,
                    role: None,
                    prepare_request: None,
                    prepared: None,
                    native: None,
                    complete_request: None,
                    completed: None,
                    copy_requests: Vec::new(),
                    copies: Vec::new(),
                    binding,
                    retirement_request: None,
                    retired: None,
                    transport_retired: false,
                    error: None,
                },
                task: None,
                scratch: None,
            })),
        })
    }
    pub(super) fn binding(&self) -> Arc<Binding> {
        self.binding.clone()
    }
    fn completion(&self, capture: Arc<Capture>) -> io::Result<Completion> {
        self.binding.custody.check_collected_capture(&capture)?;
        // Extract immutable handles while outside the shared engine lock. The
        // completed capture and each handle keep the same canonical raw owner.
        let attempts = self
            .binding
            .custody
            .completed_since(0)?
            .map(|delta| delta.native_attempts().to_vec())
            .unwrap_or_default();
        if let Some(prior) = self.binding.attempts.get() {
            if prior.len() != attempts.len() || prior.iter().zip(&attempts).any(|(a, b)| !a.same(b))
            {
                return Err(io::Error::other(
                    "helper completion changed canonical attempts",
                ));
            }
        } else {
            self.binding.attempts.set(attempts).map_err(|_| {
                io::Error::other("helper completed attempt owner changed concurrently")
            })?;
        }
        Ok(Completion {
            binding: self.binding.clone(),
            capture,
        })
    }
    pub(super) fn check_completion(&self, completion: Option<&Completion>) -> io::Result<()> {
        if completion.is_none_or(|c| !Arc::ptr_eq(&self.binding, &c.binding)) {
            return Err(io::Error::other(
                "helper completion lost its exact pending custody",
            ));
        }
        Ok(())
    }
    pub(super) fn receipt(&self) -> Receipt {
        self.state.lock().unwrap().receipt.clone()
    }
    fn fail(&self, error: &io::Error) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .receipt
            .error
            .get_or_insert_with(|| error.to_string());
    }
}

/// All pointer targets live on owned allocations before preparation. Moving
/// this struct never moves a buffer, iovec array, or the user msghdr itself.
struct Scratch {
    kind: ReceiveKind,
    count: usize,
    _sink: Vec<u8>,
    bytes: Vec<u8>,
    _iov: Vec<libc::iovec>,
    header: Box<libc::msghdr>,
}
// The pointed-to allocations belong to this value and remain stable. Only the
// one retained native worker holds the private scratch mutex for one
// synchronous syscall; no other thread dereferences or reads scratch bytes.
unsafe impl Send for Scratch {}
impl std::fmt::Debug for Scratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scratch")
            .field("kind", &self.kind)
            .field("count", &self.count)
            .field("address", &self.address())
            .field("vectors", &self._iov.len())
            .finish()
    }
}
impl Scratch {
    fn new(effect: &Effect) -> io::Result<Self> {
        let (kind, count) = match *effect {
            Effect::Drain { maximum } if (1..=DRAIN_VIEW).contains(&maximum) => {
                (ReceiveKind::Drain, maximum)
            }
            Effect::Peek { maximum } if (PUBLICATION_UNIT..=MAX_RW_COUNT).contains(&maximum) => {
                (ReceiveKind::Peek, maximum)
            }
            _ => {
                return Err(io::Error::other(
                    "helper scratch changed existing effect bounds",
                ));
            }
        };
        let mut header: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
        let mut bytes = vec![
            0;
            if kind == ReceiveKind::Drain {
                count
            } else {
                PUBLICATION_UNIT
            }
        ];
        let prefix = if kind == ReceiveKind::Peek {
            count - PUBLICATION_UNIT
        } else {
            0
        };
        let vectors = prefix
            .div_ceil(DRAIN_VIEW)
            .min(libc::UIO_MAXIOV as usize - 1);
        let sink_size = if vectors == 0 {
            0
        } else {
            prefix.div_ceil(vectors)
        };
        let mut sink = vec![0; sink_size];
        let mut iov = Vec::with_capacity(vectors + 1);
        let mut left = prefix;
        for _ in 0..vectors {
            let length = left.min(sink_size);
            iov.push(libc::iovec {
                iov_base: sink.as_mut_ptr().cast(),
                iov_len: length,
            });
            left -= length;
        }
        iov.push(libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        });
        header.msg_iov = iov.as_mut_ptr();
        header.msg_iovlen = iov.len();
        Ok(Self {
            kind,
            count,
            _sink: sink,
            bytes,
            _iov: iov,
            header,
        })
    }
    fn flags(&self) -> i32 {
        libc::MSG_DONTWAIT
            | if self.kind == ReceiveKind::Peek {
                libc::MSG_PEEK
            } else {
                0
            }
    }
    fn operation(&self) -> u64 {
        if self.kind == ReceiveKind::Drain {
            21
        } else {
            22
        }
    }
    fn address(&self) -> u64 {
        if self.kind == ReceiveKind::Drain {
            self.bytes.as_ptr() as u64
        } else {
            self.header.as_ref() as *const libc::msghdr as u64
        }
    }
    fn perform(&mut self, fd: BorrowedFd<'_>) -> CallStatus {
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3174): exact helper receive authority.
        let (name, returned) = if self.kind == ReceiveKind::Drain {
            ("helper recvfrom", unsafe {
                libc::syscall(
                    libc::SYS_recvfrom,
                    fd.as_raw_fd(),
                    self.bytes.as_mut_ptr(),
                    self.count,
                    libc::MSG_DONTWAIT,
                    std::ptr::null_mut::<libc::sockaddr>(),
                    std::ptr::null_mut::<libc::socklen_t>(),
                )
            })
        } else {
            ("helper recvmsg", unsafe {
                libc::syscall(
                    libc::SYS_recvmsg,
                    fd.as_raw_fd(),
                    self.header.as_mut() as *mut libc::msghdr,
                    self.flags(),
                )
            })
        };
        // Capture errno before a lock, allocation, format, or another syscall.
        let errno = (returned < 0).then(|| {
            io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO)
        });
        CallStatus {
            operation: name.into(),
            returned: returned as i32,
            errno,
        }
    }
}

fn retain_native(held: &Held, native: CallStatus) -> io::Result<()> {
    match held.state.lock() {
        Ok(mut state) => {
            if state.receipt.native.is_some() {
                return Err(io::Error::other(
                    "helper native result was already retained",
                ));
            }
            state.receipt.native = Some(native);
            Ok(())
        }
        Err(poisoned) => {
            // Preserve the actual return before refusing the poisoned owner.
            // Clearing poison or continuing successful publication is forbidden.
            let mut state = poisoned.into_inner();
            if state.receipt.native.is_none() {
                state.receipt.native = Some(native);
            }
            state
                .receipt
                .error
                .get_or_insert_with(|| "helper result owner was poisoned".into());
            Err(io::Error::other(
                "helper result owner was poisoned; native evidence retained",
            ))
        }
    }
}

fn kernel_return(actual: &CallStatus) -> io::Result<i64> {
    match (actual.returned, actual.errno) {
        (-1, Some(errno)) if (1..=4095).contains(&errno) => Ok(-i64::from(errno)),
        (returned, None) if returned >= 0 => Ok(i64::from(returned)),
        _ => Err(io::Error::other(
            "helper libc result/errno is not an actual native outcome",
        )),
    }
}

#[cfg(test)]
fn checked_effect(receipt: &Receipt, fd: i32) -> io::Result<OriginalEffect> {
    checked_effect_for_version(receipt, fd, 4)
}
fn checked_effect_for_version(
    receipt: &Receipt,
    fd: i32,
    version: u64,
) -> io::Result<OriginalEffect> {
    let prepared = receipt
        .prepared
        .as_ref()
        .ok_or_else(|| io::Error::other("helper preparation unknown"))?;
    let actual = receipt
        .native
        .as_ref()
        .ok_or_else(|| io::Error::other("helper syscall result unknown"))?;
    let (selection, effect) = receipt
        .completed
        .as_ref()
        .ok_or_else(|| io::Error::other("helper collection unknown"))?;
    let effect = effect
        .as_ref()
        .ok_or_else(|| io::Error::other("helper has no native protocol completion"))?;
    let Some(AuxiliaryRole::Receive {
        kind,
        address,
        count,
        provider,
        file,
    }) = receipt.role
    else {
        return Err(io::Error::other(
            "helper changed its exact prepared receive role",
        ));
    };
    if !matches!(receipt.effect,
        Effect::Drain { maximum } if kind == ReceiveKind::Drain && maximum as u64 == count)
        && !matches!(receipt.effect,
            Effect::Peek { maximum } if kind == ReceiveKind::Peek && maximum as u64 == count)
    {
        return Err(io::Error::other(
            "helper role changed the existing pending effect",
        ));
    }
    let operation = if kind == ReceiveKind::Drain { 21 } else { 22 };
    let flags = if kind == ReceiveKind::Drain {
        libc::MSG_DONTWAIT
    } else {
        libc::MSG_DONTWAIT | libc::MSG_PEEK
    };
    let native = kernel_return(actual)?;
    let selected = &selection.raw;
    receipt.role.unwrap().check_selection(
        selected,
        receipt.call.native_command_call(),
        receipt.owner.mm.generation(),
        fd,
        prepared.raw,
    )?;
    if receipt.task.is_none()
        || prepared.status.returned != 0
        || prepared.status.errno.is_some()
        || prepared.raw == 0
        || selection.status.returned != 0
        || selection.status.errno.is_some()
        || effect.status.returned != 0
        || effect.status.errno.is_some()
        || selected != &effect.raw.original.selection
        || selected.command != prepared.raw
        || selected.call != receipt.call.native_command_call()
        || selected.owner_mm != receipt.owner.mm.generation()
        || selected.requested_fd != fd
        || selected.user_address != address
        || selected.original_count != count
        || selected.address_length != flags
        || selected.fdput_flags != 0
        || selected.ready != 1
        || selected.table == 0
        || selected.task == 0
        || selected.task_start == 0
        || !receipt.identity.matches(provider, file)
        || !receipt.identity.matches(selected.provider, selected.file)
        || effect.raw.command.operation != operation
        || i64::from(effect.raw.original.returned) != native
        || native > count as i64
        || effect.raw.original.complete != 1
        || effect.raw.original.problem != 0
        || effect
            .raw
            .read_copy
            .is_none_or(|manifest| manifest.present != 1)
        || actual.operation
            != if kind == ReceiveKind::Drain {
                "helper recvfrom"
            } else {
                "helper recvmsg"
            }
    {
        return Err(io::Error::other(
            "helper actual selection/result changed held-file authority",
        ));
    }
    effect
        .raw
        .read_copy
        .unwrap()
        .validate_for_version(&effect.raw, version)?;
    Ok(effect.raw.clone())
}

fn observe(
    controller: &Controller,
    executor: &tokio::runtime::Handle,
    work: &Execution,
    held: &Held,
    quarantine: &NativeQuarantine,
) -> io::Result<NativeObservation> {
    let initial = held.receipt();
    let scratch = Scratch::new(&initial.effect)?;
    let (provider, file) = initial.identity.provider_file();
    let role = AuxiliaryRole::Receive {
        kind: scratch.kind,
        address: scratch.address(),
        count: scratch.count as u64,
        provider,
        file,
    };
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tid, libc::O_EXCL) };
    if pidfd < 0 {
        return Err(io::Error::last_os_error());
    }
    let task = Arc::new(unsafe { OwnedFd::from_raw_fd(pidfd as i32) });
    let task_identity = PidfdIdentity::read(&task)?;
    {
        let mut state = held.state.lock().unwrap();
        if state.task.is_some()
            || state.scratch.is_some()
            || state.receipt.prepare_request.is_some()
        {
            return Err(io::Error::other("helper worker preparation repeated"));
        }
        state.task = Some(task.clone());
        state.scratch = Some(Arc::new(Mutex::new(scratch)));
        state.receipt.task = Some(task_identity);
        state.receipt.role = Some(role);
    }
    // Latch possible provider task custody before any transport effect. Only
    // exact auxiliary retirement below can make this worker reusable.
    quarantine.mark_possible()?;
    let prepared = controller.prepare(
        ControllerEffect::PrepareOriginalFileObservation(initial.call),
        initial.owner,
        &Request::PrepareOriginalFileObservation {
            call: initial.call.native_command_call(),
            mm: initial.owner.mm.generation(),
            fd: work.original.as_raw_fd(),
            role,
        },
        || Ok(vec![task.as_fd().try_clone_to_owned()?]),
    )?;
    held.state.lock().unwrap().receipt.prepare_request = Some(prepared);
    let Reply::Prepared(observed) = executor.block_on(controller.response(prepared))? else {
        return Err(io::Error::other("helper preparation changed response"));
    };
    held.state.lock().unwrap().receipt.prepared = Some(observed.clone());
    if observed.status.returned != 0 || observed.status.errno.is_some() || observed.raw == 0 {
        return Err(io::Error::other(
            "helper preparation lacks exact retained command",
        ));
    }
    let prepared_wire =
        controller.prepared_copy_authority(initial.owner, initial.call, prepared, observed.raw)?;
    held.binding
        .custody
        .bind(initial.owner, initial.call, observed.raw)?;
    held.binding.custody.retain_preparation(prepared_wire)?;
    let scratch = held
        .state
        .lock()
        .unwrap()
        .scratch
        .as_ref()
        .cloned()
        .ok_or_else(|| io::Error::other("helper lost owned stable scratch"))?;
    // The same Held state retains this allocation throughout native entry and
    // every unwind. This private scratch mutex is not a scheduler, engine or
    // runtime registry lock; the provider only observes stable raw addresses.
    let native = scratch.lock().unwrap().perform(work.original.as_fd());
    retain_native(held, native)?;
    let completed = controller.prepare(
        ControllerEffect::CollectOriginalFileObservation(initial.call),
        initial.owner,
        &Request::CollectOriginalFileObservation {
            call: initial.call.native_command_call(),
            command: observed.raw,
            prepared_request: prepared,
            role,
        },
        || Ok(vec![]),
    )?;
    held.state.lock().unwrap().receipt.complete_request = Some(completed);
    let Reply::OriginalFileObservation { selection, effect } =
        executor.block_on(controller.response(completed))?
    else {
        return Err(io::Error::other("helper collection changed response"));
    };
    held.state.lock().unwrap().receipt.completed = Some((selection, effect));
    let prepared_wire = held.binding.custody.take_preparation()?;
    let wire = controller.bind_copy_authority(prepared_wire, completed)?;
    let version = wire.version();
    held.binding.custody.prepare(wire)?;
    let effect = checked_effect_for_version(&held.receipt(), work.original.as_raw_fd(), version)?;
    loop {
        let first = held.binding.custody.len()? as u64;
        let sequence = controller.prepare(
            ControllerEffect::ReadOriginalCopy(initial.call, first),
            initial.owner,
            &Request::ReadOriginalCopy {
                call: initial.call.native_command_call(),
                command: observed.raw,
                prepared,
                first,
            },
            || Ok(vec![]),
        )?;
        held.state
            .lock()
            .unwrap()
            .receipt
            .copy_requests
            .push((first, sequence));
        let Reply::OriginalReadCopy(chunk) = executor.block_on(controller.response(sequence))?
        else {
            return Err(io::Error::other("helper copy response changed kind"));
        };
        let end = chunk.end;
        // Envelope diagnostics do not duplicate payload. Canonical custody
        // takes every actual raw frame before envelope or parser validation.
        held.state
            .lock()
            .unwrap()
            .receipt
            .copies
            .push(CopyEnvelope {
                sequence,
                prepared: chunk.prepared,
                first: chunk.first,
                records: chunk.records.len(),
                end,
            });
        held.binding.custody.append_helper_chunk(
            &effect.original.selection,
            prepared,
            first,
            chunk,
        )?;
        if let Some(end) = end {
            if end != (super::original_read_copy::End::OriginalExit { protocol: true }) {
                return Err(io::Error::other("helper copy lacks actual protocol EXIT"));
            }
            break;
        }
    }
    let capture = held.binding.custody.collect(&effect)?;
    let completion = held.completion(capture.clone())?;
    let actual = held.receipt().native.unwrap();
    let mut bytes = capture.committed.clone();
    let confirmation = if let Some(errno) = actual.errno {
        if !bytes.is_empty() {
            return Err(io::Error::other(
                "negative helper has committed positive bytes",
            ));
        }
        ResultValue::Errno(errno)
    } else {
        match initial.effect {
            Effect::Drain { .. } => ResultValue::Drained {
                bytes: bytes.clone(),
            },
            Effect::Peek { maximum } => {
                // Preserve the existing bounded suffix behavior, using authenticated
                // iterator positions instead of rereading aliased scratch iovecs.
                let prefix = maximum - PUBLICATION_UNIT;
                bytes = bytes.get(prefix..).unwrap_or(&[]).to_vec();
                ResultValue::Peeked {
                    count: actual.returned as usize,
                }
            }
            _ => {
                return Err(io::Error::other(
                    "helper effect changed during native execution",
                ));
            }
        }
    };
    let retired = controller.prepare(
        ControllerEffect::RetireOriginalFileObservation(initial.call),
        initial.owner,
        &Request::RetireOriginalFileObservation {
            call: initial.call.native_command_call(),
            prepared,
            completed,
        },
        || Ok(vec![]),
    )?;
    held.state.lock().unwrap().receipt.retirement_request = Some(retired);
    let Reply::OriginalFileObservationRetired(retirement) =
        executor.block_on(controller.response(retired))?
    else {
        return Err(io::Error::other("helper retirement changed response"));
    };
    held.state.lock().unwrap().receipt.retired = Some(retirement.clone());
    if retirement.returned != 0 || retirement.errno.is_some() {
        return Err(io::Error::other(
            "helper worker auxiliary registration remains owned",
        ));
    }
    controller.retire_file_observation(
        initial.owner,
        initial.call,
        [prepared, completed, retired],
    )?;
    held.state.lock().unwrap().receipt.transport_retired = true;
    quarantine.retired();
    Ok(NativeObservation {
        raw_return: i64::from(actual.returned),
        errno: actual.errno,
        bytes,
        confirmation,
        helper_copy: Some(completion),
    })
}

impl RuntimeShared {
    pub(super) async fn execute_helper_receive(
        self: &Arc<Self>,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        effect: Effect,
    ) -> io::Result<NativeObservation> {
        self.execute_helper_receive_inner(owner, lease, effect, None)
            .await
    }

    pub(super) async fn execute_private_receive_drain(
        self: &Arc<Self>,
        full: crate::network_replay::FullStoreCompletion,
    ) -> io::Result<NativeObservation> {
        let store = full.store();
        self.execute_helper_receive_inner(
            store.owner(),
            store.lease(),
            Effect::Drain {
                maximum: store.length(),
            },
            Some(full),
        )
        .await
    }

    async fn execute_helper_receive_inner(
        self: &Arc<Self>,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        effect: Effect,
        full: Option<crate::network_replay::FullStoreCompletion>,
    ) -> io::Result<NativeObservation> {
        let controller = self
            .controller
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| io::Error::other("helper receive lacks provider controller"))?
            .map_err(io::Error::other)?;
        let shared = self.clone();
        let executor = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let worker_executor = executor.clone();
        let quarantine = Arc::new(NativeQuarantine::default());
        let worker_quarantine = quarantine.clone();
        let worker_effect = effect.clone();
        let (worker, receive) = self.start_native_worker_with_quarantine(
            executor,
            None,
            false,
            Some(quarantine.clone()),
            move || {
                let effect = worker_effect;
                let work = shared.native_streams.lock().unwrap().prepare_with_store(
                    owner,
                    lease,
                    effect.clone(),
                    full.as_ref(),
                )?;
                let held = work
                    .helper
                    .clone()
                    .ok_or_else(|| io::Error::other("helper lacks existing Pending owner"))?;
                worker_quarantine.retain(held.clone())?;
                let result = (|| {
                    // Extracted owned handles: no native registry/Held/custody lock
                    // is held while joining the already-submitted engine Call.
                    let publication = work.publication.as_ref().ok_or_else(|| {
                        io::Error::other("helper lacks captured shared-engine owner")
                    })?;
                    let mut engine = publication.engine().lock().unwrap();
                    if let Some(full) = &full {
                        engine
                            .bind_private_drain_helper(full, held.binding())
                            .map_err(io::Error::other)?;
                    } else {
                        engine
                            .bind_helper_copy(held.binding())
                            .map_err(io::Error::other)?;
                    }
                    drop(engine);
                    observe(
                        &controller,
                        &worker_executor,
                        &work,
                        &held,
                        &worker_quarantine,
                    )
                })();
                match &result {
                    Ok(result) => shared.native_streams.lock().unwrap().retain(
                        owner,
                        lease,
                        &effect,
                        result.clone(),
                    )?,
                    Err(error) => {
                        held.fail(error);
                        shared
                            .native_terminal_failure
                            .lock()
                            .unwrap()
                            .get_or_insert_with(|| error.to_string());
                    }
                }
                result
            },
        )?;
        let result = receive.await.map_err(io::Error::other)?;
        // Failure was delivered before the same worker parks. Awaiting that
        // handle here would prevent the caller reaching bounded shutdown.
        if quarantine.is_possible() {
            return Err(result
                .err()
                .unwrap_or_else(|| io::Error::other("quarantined helper cannot return success")));
        }
        let joined = self.join_native_worker_receipt(&worker).await?;
        if let Ok(observed) = &result {
            // Do not use the success callback as a join receipt. The actual
            // retained worker has ended; only the same Pending/result may own it.
            retain_joined_helper(self, owner, lease, &effect, observed, joined)?;
        }
        result
    }
}

pub(super) fn retain_joined_helper(
    shared: &Arc<RuntimeShared>,
    owner: NetworkStreamOwner,
    lease: NetworkStreamLeaseId,
    effect: &Effect,
    observed: &NativeObservation,
    joined: JoinedNativeWorkerReceipt,
) -> io::Result<()> {
    joined.validate_runtime(shared)?;
    // Keep the exact Pending live through attachment; terminal cleanup cannot
    // remove the slot between a successful preflight and its one-time receipt.
    let calls = shared.native_streams.lock().unwrap();
    calls.preflight_confirmation(owner, lease, effect, observed)?;
    let completion = observed
        .helper_copy
        .as_ref()
        .ok_or_else(|| io::Error::other("joined helper lost canonical completion"))?;
    completion
        .binding
        .joined
        .set(joined)
        .map_err(|_| io::Error::other("helper worker receipt was already assigned"))?;
    drop(calls);
    Ok(())
}

/// Controlled provider metadata supplements existing native Unix byte/errno
/// oracles in component tests. These records are never native BPF evidence.
#[cfg(test)]
impl Held {
    // Existing scratch/result controls predate lease custody; retain their exact
    // bodies and provide only this controlled fixture's previously absent lease.
    fn new(
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        identity: FileIdentity,
        effect: Effect,
    ) -> io::Result<Self> {
        Self::new_bound(
            owner,
            call,
            serde_json::from_value(serde_json::json!(11))?,
            identity,
            effect,
        )
    }

    pub(super) fn controlled_observation(
        &self,
        observed: NativeObservation,
        version: u64,
    ) -> io::Result<NativeObservation> {
        self.controlled_observation_at(observed, version, 0, 0)
    }
    fn controlled_observation_at(
        &self,
        mut observed: NativeObservation,
        version: u64,
        before: u64,
        order: u64,
    ) -> io::Result<NativeObservation> {
        use super::original_read_copy::Chunk;
        use super::original_read_copy::End;
        use super::original_read_copy::Manifest;
        use super::original_read_copy::RECORD_BYTES;
        use super::original_read_copy::Record;
        use super::original_read_copy::Summary;
        let binding = &self.binding;
        let (operation, flags, count) = match binding.effect {
            Effect::Drain { maximum } => (21, 0x40, maximum),
            Effect::Peek { maximum } => (22, 0x42, maximum),
            _ => {
                return Err(io::Error::other(
                    "controlled helper requires receive effect",
                ));
            }
        };
        // The old native component fixtures inspect one wholly retained prefix.
        // They do not provide queue geometry; V5 geometry below is explicitly
        // controlled input, and only new tests request that grammar.
        if observed.bytes.len() > 512 || observed.raw_return.max(0) as usize != observed.bytes.len()
        {
            return Err(io::Error::other(
                "controlled helper fixture requires its complete small prefix",
            ));
        }
        let (provider, file) = binding.identity.provider_file();
        let selected = OriginalSelection {
            provider,
            command: 11,
            call: binding.call.native_command_call(),
            task: 17,
            task_start: 19,
            table: 23,
            file,
            user_address: 0x1000,
            ready: 1,
            requested_fd: 29,
            original_count: count as u64,
            owner_mm: binding.owner.mm.generation(),
            fdput_flags: 0,
            address_length: flags,
        };
        binding
            .custody
            .bind(binding.owner, binding.call, selected.command)?;
        let wire = super::copy_wire_authority::controlled_copy_authority_with_preparation(
            match version {
                4 => ProviderWireFormat::Abi7Copy4,
                5 => ProviderWireFormat::Abi8Copy5,
                _ => return Err(io::Error::other("controlled helper unknown wire")),
            },
            binding.owner,
            operation,
            selected.clone(),
            |prepared| {
                binding.custody.retain_preparation(prepared)?;
                binding.custody.take_preparation()
            },
        )?;
        binding.custody.prepare(wire)?;
        let disposition = if operation == 21 { 1 } else { 2 };
        let copied = observed.bytes.len() as u64;
        let mut records = Vec::new();
        let mut record = |kind: u32, fields: &[u64], payload: Option<&[u8]>| {
            let mut bytes = vec![0; RECORD_BYTES];
            for (word, value) in bytes.chunks_exact_mut(8).zip(fields) {
                word.copy_from_slice(&value.to_le_bytes());
            }
            let length = if let Some(payload) = payload {
                bytes[..payload.len()].copy_from_slice(payload);
                payload.len()
            } else {
                fields.len() * 8
            };
            records.push(Record {
                provider,
                command: selected.command,
                call: selected.call,
                task: selected.task,
                task_start: selected.task_start,
                sequence: records.len() as u64 + 1,
                attempt: 1,
                offset: 0,
                length: length as u32,
                kind,
                bytes,
            });
        };
        if copied > 0 {
            if version == 5 {
                record(
                    4,
                    &[
                        file,
                        before,
                        before,
                        order,
                        0,
                        copied,
                        copied,
                        0,
                        copied,
                        0,
                        42,
                        1,
                        disposition,
                    ],
                    None,
                );
            }
            record(1, &[], Some(&observed.bytes));
            if version == 5 {
                record(
                    5,
                    &[
                        file,
                        order + u64::from(operation == 21),
                        0,
                        copied,
                        copied,
                        0,
                        42,
                        1,
                        disposition,
                        before,
                        if operation == 21 {
                            before + copied
                        } else {
                            before
                        },
                    ],
                    None,
                );
            } else {
                record(
                    3,
                    &[
                        file,
                        u64::from(operation == 21),
                        0,
                        copied,
                        copied,
                        0,
                        42,
                        1,
                        disposition,
                    ],
                    None,
                );
            }
        }
        let mut raw = super::accepted_provider_ffi::OriginalEffect::default();
        let returned = observed
            .errno
            .map_or(observed.raw_return, |errno| -i64::from(errno));
        raw.command.operation = operation;
        raw.command.command = selected.command;
        raw.command.phase = 1;
        raw.command.returned = returned as i32;
        raw.command.identity.provider = provider;
        raw.command.task = selected.task;
        raw.command.start_boottime = selected.task_start;
        raw.command.original_count = count as u64;
        raw.original.complete = 1;
        raw.original.returned = returned as i32;
        let mut effect: OriginalEffect = raw.into();
        effect.original.selection = selected.clone();
        effect.read_copy = Some(Manifest {
            provider,
            command: selected.command,
            call: selected.call,
            task: selected.task,
            task_start: selected.task_start,
            present: 1,
            returned,
            summary: Summary {
                version,
                initial_count: count as u64,
                attempts: u64::from(copied > 0),
                records: records.len() as u64,
                copied,
                final_count: count as u64 - copied,
                protocol_returned: returned as u64,
                protocol_complete: 1,
            },
        });
        for r in records {
            let first = binding.custody.len()? as u64;
            binding.custody.append_helper_chunk(
                &selected,
                1,
                first,
                Chunk {
                    prepared: 1,
                    first,
                    records: vec![r],
                    end: None,
                },
            )?;
        }
        let first = binding.custody.len()? as u64;
        binding.custody.append_helper_chunk(
            &selected,
            1,
            first,
            Chunk {
                prepared: 1,
                first,
                records: vec![],
                end: Some(End::OriginalExit { protocol: true }),
            },
        )?;
        observed.helper_copy = Some(self.completion(binding.custody.collect(&effect)?)?);
        Ok(observed)
    }
}
#[cfg(test)]
impl Binding {
    pub(crate) fn controlled_fixture(
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        effect: Effect,
    ) -> Arc<Self> {
        Held::new_bound(
            owner,
            call,
            lease,
            FileIdentity::controlled_fixture(3, 7),
            effect,
        )
        .unwrap()
        .binding()
    }
}

#[cfg(test)]
mod tests {
    use super::super::accepted_provider_ffi as ffi;
    use super::*;

    fn owner() -> NetworkStreamOwner {
        let thread = crate::types::DetTid::from_raw(7);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }
    fn receipt(kind: ReceiveKind, returned: i32) -> Receipt {
        let call = NetworkStreamCallId::controlled_fixture(5);
        let count = if kind == ReceiveKind::Drain {
            512
        } else {
            1024
        };
        let effect = if kind == ReceiveKind::Drain {
            Effect::Drain { maximum: count }
        } else {
            Effect::Peek { maximum: count }
        };
        let mut receipt = Held::new(
            owner(),
            call,
            FileIdentity::controlled_fixture(3, 7),
            effect,
        )
        .unwrap()
        .receipt();
        // This component owns an actual task capability; the provider effect
        // below remains explicit controlled input, not a native BPF claim.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tid, libc::O_EXCL) };
        assert!(pidfd >= 0);
        let task = unsafe { OwnedFd::from_raw_fd(pidfd as i32) };
        receipt.task = Some(PidfdIdentity::read(&task).unwrap());
        receipt.role = Some(AuxiliaryRole::Receive {
            kind,
            address: 0x1000,
            count: count as u64,
            provider: 3,
            file: 7,
        });
        let ok = CallStatus {
            operation: "component provider success".into(),
            returned: 0,
            errno: None,
        };
        receipt.prepared = Some(Observation {
            status: ok.clone(),
            raw: 11,
        });
        receipt.native = Some(CallStatus {
            operation: if kind == ReceiveKind::Drain {
                "helper recvfrom"
            } else {
                "helper recvmsg"
            }
            .into(),
            returned: if returned < 0 { -1 } else { returned },
            errno: (returned < 0).then_some(-returned),
        });
        let mut raw = ffi::OriginalEffect::default();
        raw.command.operation = if kind == ReceiveKind::Drain { 21 } else { 22 };
        raw.command.command = 11;
        raw.command.phase = 1;
        raw.command.identity.provider = 3;
        raw.command.task = 17;
        raw.command.start_boottime = 19;
        raw.command.original_count = count as u64;
        raw.command.returned = returned;
        raw.original.selection = ffi::OriginalSelection {
            command: 11,
            call: call.native_command_call(),
            owner_mm: owner().mm.generation(),
            provider: 3,
            task: 17,
            task_start: 19,
            table: 23,
            file: 7,
            user_address: 0x1000,
            ready: 1,
            requested_fd: 29,
            address_length: if kind == ReceiveKind::Drain {
                0x40
            } else {
                0x42
            },
            original_count: count as u64,
            ..Default::default()
        };
        raw.original.complete = 1;
        raw.original.returned = returned;
        let mut effect: OriginalEffect = raw.into();
        effect.read_copy = Some(super::super::original_read_copy::Manifest {
            provider: 3,
            command: 11,
            call: call.native_command_call(),
            task: 17,
            task_start: 19,
            present: 1,
            returned: i64::from(returned),
            summary: super::super::original_read_copy::Summary {
                version: 4,
                initial_count: count as u64,
                final_count: count as u64 - returned.max(0) as u64,
                protocol_returned: i64::from(returned) as u64,
                protocol_complete: 1,
                ..Default::default()
            },
        });
        receipt.completed = Some((
            Observation {
                status: ok.clone(),
                raw: effect.original.selection.clone(),
            },
            Some(Observation {
                status: ok,
                raw: effect,
            }),
        ));
        receipt
    }

    #[test]
    fn helper_scratch_keeps_exact_pointer_and_iterator_operands_across_moves() {
        for maximum in [1024, 1025, 2048, MAX_RW_COUNT] {
            let scratch = Scratch::new(&Effect::Peek { maximum }).unwrap();
            let address = scratch.address();
            let iov = scratch.header.msg_iov;
            assert_eq!(
                scratch._iov.iter().map(|v| v.iov_len).sum::<usize>(),
                maximum
            );
            assert_eq!(scratch.bytes.len(), PUBLICATION_UNIT);
            assert!(scratch.header.msg_name.is_null());
            assert_eq!(scratch.header.msg_namelen, 0);
            assert!(scratch.header.msg_control.is_null());
            assert_eq!(scratch.header.msg_controllen, 0);
            assert_eq!(scratch.header.msg_flags, 0);
            assert_eq!(scratch.flags(), 0x42);
            let moved = vec![Box::new(scratch)];
            assert_eq!(moved[0].address(), address);
            assert_eq!(moved[0].header.msg_iov, iov);
            assert_eq!(moved[0]._iov.len(), moved[0].header.msg_iovlen);
        }
        for maximum in [1, DRAIN_VIEW] {
            let scratch = Scratch::new(&Effect::Drain { maximum }).unwrap();
            assert_eq!(scratch.address(), scratch.bytes.as_ptr() as u64);
            assert_eq!(scratch.count, maximum);
            assert_eq!(scratch.operation(), 21);
            assert_eq!(scratch.flags(), 0x40);
        }
        for effect in [
            Effect::Drain { maximum: 0 },
            Effect::Drain {
                maximum: DRAIN_VIEW + 1,
            },
            Effect::Peek {
                maximum: PUBLICATION_UNIT - 1,
            },
            Effect::Peek {
                maximum: MAX_RW_COUNT + 1,
            },
        ] {
            assert!(Scratch::new(&effect).is_err());
        }
    }

    #[test]
    fn helper_scratch_and_task_survive_native_return_poison_and_call_removal() {
        use std::io::Write;
        let (receiver, mut sender) = std::os::unix::net::UnixStream::pair().unwrap();
        sender.write_all(b"abc").unwrap();
        let held = Held::new(
            owner(),
            NetworkStreamCallId::controlled_fixture(9),
            FileIdentity::controlled_fixture(3, 7),
            Effect::Drain { maximum: 3 },
        )
        .unwrap();
        let scratch = Arc::new(Mutex::new(
            Scratch::new(&Effect::Drain { maximum: 3 }).unwrap(),
        ));
        let address = scratch.lock().unwrap().address();
        let weak_scratch = Arc::downgrade(&scratch);
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tid, libc::O_EXCL) };
        assert!(pidfd >= 0);
        let task = Arc::new(unsafe { OwnedFd::from_raw_fd(pidfd as i32) });
        let task_fd = task.as_raw_fd();
        let weak_task = Arc::downgrade(&task);
        {
            let mut state = held.state.lock().unwrap();
            state.task = Some(task.clone());
            state.scratch = Some(scratch.clone());
        }
        let quarantine = NativeQuarantine::default();
        quarantine.retain(held.clone()).unwrap();
        // Controlled possibly-registered premise: no BPF operation is used by
        // this ownership test, but perform executes the real one-shot receive.
        quarantine.mark_possible().unwrap();
        let actual = scratch.lock().unwrap().perform(receiver.as_fd());
        assert_eq!(actual.returned, 3);
        assert_eq!(actual.errno, None);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _locked = held.state.lock().unwrap();
                panic!("component: reader poisons State after actual native return");
            }))
            .is_err()
        );
        assert!(retain_native(&held, actual.clone()).is_err());
        {
            let state = held.state.lock().unwrap_err().into_inner();
            assert_eq!(state.receipt.native.as_ref(), Some(&actual));
            assert!(state.receipt.error.is_some());
        }
        drop(task);
        drop(scratch);
        drop(held); // callback and Call have disappeared
        let still_scratch = weak_scratch
            .upgrade()
            .expect("parked worker must own actual allocations");
        assert_eq!(still_scratch.lock().unwrap().address(), address);
        assert_eq!(still_scratch.lock().unwrap().bytes, b"abc");
        assert!(weak_task.upgrade().is_some());
        assert!(unsafe { libc::fcntl(task_fd, libc::F_GETFD) } >= 0);
        assert!(quarantine.is_possible());
    }

    #[test]
    fn helper_completion_requires_held_identity_native_errno_and_protocol_even_without_payload() {
        for kind in [ReceiveKind::Drain, ReceiveKind::Peek] {
            for returned in [0, -libc::EAGAIN, -libc::EINTR] {
                let receipt = receipt(kind, returned);
                assert_eq!(
                    checked_effect(&receipt, 29).unwrap().original.returned,
                    returned
                );
                for mutation in 0..16 {
                    let mut bad = receipt.clone();
                    match mutation {
                        0 => bad.identity = FileIdentity::controlled_fixture(3, 8),
                        1 => bad.prepared.as_mut().unwrap().status.errno = Some(libc::EIO),
                        2 => bad.prepared.as_mut().unwrap().raw += 1,
                        3 => bad.native.as_mut().unwrap().returned = -2,
                        4 => bad.native.as_mut().unwrap().errno = Some(0),
                        5 => bad.native.as_mut().unwrap().operation = "another syscall".into(),
                        6 => bad.effect = Effect::QueuedBytes,
                        7 => bad.completed.as_mut().unwrap().0.raw.fdput_flags = 1,
                        8 => bad.completed.as_mut().unwrap().0.raw.owner_mm += 1,
                        9 => bad.completed.as_mut().unwrap().0.raw.file = 8,
                        10 => {
                            bad.completed
                                .as_mut()
                                .unwrap()
                                .1
                                .as_mut()
                                .unwrap()
                                .raw
                                .command
                                .operation = 23
                        }
                        11 => {
                            bad.completed
                                .as_mut()
                                .unwrap()
                                .1
                                .as_mut()
                                .unwrap()
                                .raw
                                .read_copy = None
                        }
                        12 => {
                            bad.completed
                                .as_mut()
                                .unwrap()
                                .1
                                .as_mut()
                                .unwrap()
                                .raw
                                .read_copy
                                .as_mut()
                                .unwrap()
                                .present = 0
                        }
                        13 => {
                            bad.completed
                                .as_mut()
                                .unwrap()
                                .1
                                .as_mut()
                                .unwrap()
                                .raw
                                .original
                                .complete = 0
                        }
                        14 => {
                            bad.completed
                                .as_mut()
                                .unwrap()
                                .1
                                .as_mut()
                                .unwrap()
                                .raw
                                .original
                                .returned += 1
                        }
                        15 => bad.task = None,
                        _ => unreachable!(),
                    }
                    assert!(
                        checked_effect(&bad, 29).is_err(),
                        "{kind:?}/{returned}/mutation {mutation}"
                    );
                }
                assert!(checked_effect(&receipt, 30).is_err());
            }
        }
    }

    fn controlled_helper(effect: Effect) -> Held {
        Held::new_bound(
            owner(),
            NetworkStreamCallId::controlled_fixture(5),
            serde_json::from_value(serde_json::json!(11)).unwrap(),
            FileIdentity::controlled_fixture(3, 7),
            effect,
        )
        .unwrap()
    }
    fn plain_helper(effect: &Effect, bytes: &[u8], errno: Option<i32>) -> NativeObservation {
        NativeObservation {
            raw_return: if errno.is_some() {
                -1
            } else {
                bytes.len() as i64
            },
            errno,
            bytes: bytes.to_vec(),
            helper_copy: None,
            confirmation: errno.map_or_else(
                || match effect {
                    Effect::Drain { .. } => ResultValue::Drained {
                        bytes: bytes.to_vec(),
                    },
                    Effect::Peek { .. } => ResultValue::Peeked { count: bytes.len() },
                    _ => unreachable!(),
                },
                ResultValue::Errno,
            ),
        }
    }
    #[test]
    fn helper_canonical_capture_keeps_actual_authority_and_v5_attempt_after_callback_drop() {
        for version in [4, 5] {
            for effect in [Effect::Drain { maximum: 3 }, Effect::Peek { maximum: 1024 }] {
                let held = controlled_helper(effect.clone());
                let binding = held.binding();
                let weak = Arc::downgrade(&binding.custody);
                let observed = held
                    .controlled_observation(plain_helper(&effect, b"abc", None), version)
                    .unwrap();
                let completion = observed.helper_copy.as_ref().unwrap();
                let capture = completion.capture.clone();
                assert_eq!(capture.committed, b"abc");
                assert_eq!(capture.manifest.summary.version, version);
                assert_eq!(capture.units.len(), 1);
                assert!(binding.custody.has_wire_authority().unwrap());
                assert_eq!(capture.units[0].observation.is_some(), version == 5);
                if let Some(attempt) = capture.units[0].observation {
                    assert_eq!((attempt.begin.requested, attempt.begin.available), (3, 3));
                    assert_eq!(
                        attempt.after,
                        if matches!(effect, Effect::Drain { .. }) {
                            3
                        } else {
                            0
                        }
                    );
                }
                let delta = binding.custody.completed_since(0).unwrap().unwrap();
                delta
                    .with_unit(0, |unit, raw| {
                        assert_eq!(unit, &capture.units[0]);
                        assert_eq!(raw, capture.records);
                        assert_eq!(raw.as_ptr(), capture.records.as_ptr());
                    })
                    .unwrap();
                // Same bytes allocated elsewhere cannot become canonical authority.
                assert!(held.completion(Arc::new((*capture).clone())).is_err());
                drop(observed);
                drop(held);
                assert!(weak.upgrade().is_some());
                binding.custody.check_collected_capture(&capture).unwrap();
                assert!(binding.custody.require_no_unjoined_receipts(1).is_err());
            }
        }
    }
    #[test]
    fn helper_empty_eof_and_errno_receipts_keep_protocol_custody_without_semantic_discharge() {
        for version in [4, 5] {
            for errno in [None, Some(libc::EAGAIN), Some(libc::EINTR)] {
                let effect = Effect::Drain { maximum: 1 };
                let held = controlled_helper(effect.clone());
                let observed = held
                    .controlled_observation(plain_helper(&effect, b"", errno), version)
                    .unwrap();
                let completion = observed.helper_copy.as_ref().unwrap();
                assert!(completion.capture.records.is_empty());
                assert!(completion.capture.units.is_empty());
                assert_eq!(completion.capture.manifest.present, 1);
                assert_eq!(
                    completion.capture.manifest.returned,
                    errno.map_or(0, |e| -i64::from(e))
                );
                assert!(
                    held.binding
                        .custody
                        .require_no_unjoined_receipts(0)
                        .is_err()
                );
                let wire = serde_json::to_vec(&observed).unwrap();
                let decoded: NativeObservation = serde_json::from_slice(&wire).unwrap();
                assert_eq!(decoded.bytes, observed.bytes);
                assert_eq!(decoded.confirmation, observed.confirmation);
                assert!(decoded.helper_copy.is_none());
                assert_ne!(decoded, observed);
                assert!(held.check_completion(decoded.helper_copy.as_ref()).is_err());
            }
        }
    }
    #[test]
    fn helper_opaque_completion_rejects_same_numbers_in_a_different_pending_or_capture() {
        let effect = Effect::Drain { maximum: 3 };
        let first = controlled_helper(effect.clone());
        let second = controlled_helper(effect.clone());
        let actual = first
            .controlled_observation(plain_helper(&effect, b"abc", None), 4)
            .unwrap();
        let other = second
            .controlled_observation(plain_helper(&effect, b"abc", None), 4)
            .unwrap();
        assert_eq!(actual.bytes, other.bytes);
        assert_eq!(actual.confirmation, other.confirmation);
        assert_ne!(actual, other);
        assert!(first.check_completion(other.helper_copy.as_ref()).is_err());
        let original = actual.helper_copy.unwrap();
        let replaced = Completion {
            binding: original.binding.clone(),
            capture: Arc::new((*original.capture).clone()),
        };
        assert_ne!(original, replaced);
        let receipt = first.receipt();
        let custody = Arc::downgrade(&receipt.binding.custody);
        drop(first);
        drop(original);
        drop(replaced);
        assert!(custody.upgrade().is_some()); // terminal receipt itself owns canonical custody
    }
}

/// The bytes/errno below are actual bounded Unix socket effects. Provider rows,
/// TCP geometry and installed file identity are explicitly controlled inputs;
/// this is not native BPF evidence or a supported TCP/Unix classification test.
#[cfg(test)]
impl Binding {
    pub(crate) async fn controlled_joined_peek(
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        bytes: &[u8],
        version: u64,
        join: bool,
        eof: bool,
    ) -> (
        NetworkRuntimeResources,
        NativeObservation,
        JoinedNativeWorkerReceipt,
    ) {
        Self::controlled_joined_peek_prefix(
            owner, call, lease, engine, bytes, version, join, eof, 0,
        )
        .await
    }
    // A nonzero sink prefix is explicit controlled provider DATA. The actual
    // syscall's entire return/errno and returned suffix remain checked below;
    // no native BPF or actual sink-buffer readback is claimed by this fixture.
    pub(crate) async fn controlled_joined_peek_prefix(
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        bytes: &[u8],
        version: u64,
        join: bool,
        eof: bool,
        prefix: usize,
    ) -> (
        NetworkRuntimeResources,
        NativeObservation,
        JoinedNativeWorkerReceipt,
    ) {
        let (runtime, _) = super::tests::fixture(121);
        Self::controlled_joined_peek_in_runtime(
            runtime, owner, call, lease, engine, bytes, version, join, eof, prefix,
        )
        .await
    }
    pub(crate) async fn controlled_joined_peek_on(
        runtime: NetworkRuntimeResources,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        bytes: &[u8],
        version: u64,
        join: bool,
        eof: bool,
    ) -> (
        NetworkRuntimeResources,
        NativeObservation,
        JoinedNativeWorkerReceipt,
    ) {
        Self::controlled_joined_peek_in_runtime(
            runtime, owner, call, lease, engine, bytes, version, join, eof, 0,
        )
        .await
    }
    pub(crate) async fn controlled_joined_peek_in_runtime(
        runtime: NetworkRuntimeResources,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        bytes: &[u8],
        version: u64,
        join: bool,
        eof: bool,
        prefix: usize,
    ) -> (
        NetworkRuntimeResources,
        NativeObservation,
        JoinedNativeWorkerReceipt,
    ) {
        Self::controlled_joined_peek_retaining_peer(
            runtime, owner, call, lease, engine, bytes, version, join, eof, prefix, None,
        )
        .await
    }
    pub(crate) async fn controlled_joined_peek_retaining_peer(
        runtime: NetworkRuntimeResources,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        lease: NetworkStreamLeaseId,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        bytes: &[u8],
        version: u64,
        join: bool,
        eof: bool,
        prefix: usize,
        keep_peer: Option<&mut Option<std::os::unix::net::UnixStream>>,
    ) -> (
        NetworkRuntimeResources,
        NativeObservation,
        JoinedNativeWorkerReceipt,
    ) {
        use std::io::Write;
        assert!(prefix <= bytes.len());
        assert!(
            bytes.len() <= DRAIN_VIEW,
            "controlled copy records preserve the existing512-byte bound"
        );
        let (original, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        peer.write_all(bytes).unwrap();
        if eof {
            peer.shutdown(std::net::Shutdown::Write).unwrap();
        }
        let effect = Effect::Peek {
            maximum: prefix + 1024,
        };
        let work = {
            let mut calls = runtime.shared.native_streams.lock().unwrap();
            calls
                .capture_authenticated(
                    owner,
                    call,
                    original.into(),
                    FileIdentity::controlled_fixture(7, 19),
                )
                .unwrap();
            calls.bind_lease(owner, call, lease).unwrap();
            calls.prepare(owner, lease, effect.clone()).unwrap()
        };
        let held = work.helper.as_ref().unwrap().clone();
        engine.bind_helper_copy(held.binding()).unwrap();
        let shared = runtime.shared.clone();
        let worker_effect = effect.clone();
        let controlled_full = bytes.to_vec();
        let (worker, reply) = runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), move || {
                let mut observed = work.perform();
                assert_eq!(observed.bytes, controlled_full[prefix..]);
                let actual_suffix = std::mem::replace(&mut observed.bytes, controlled_full);
                let mut observed = held.controlled_observation(observed, version)?;
                observed.bytes = actual_suffix;
                shared.native_streams.lock().unwrap().retain(
                    owner,
                    lease,
                    &worker_effect,
                    observed.clone(),
                )?;
                Ok(observed)
            })
            .unwrap();
        let observed = tokio::time::timeout(std::time::Duration::from_secs(1), reply)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if bytes.is_empty() && !eof {
            assert_eq!(observed.raw_return, -1);
            assert_eq!(observed.errno, Some(libc::EAGAIN));
        } else {
            assert_eq!(observed.raw_return, bytes.len() as i64);
            assert_eq!(observed.errno, None);
        }
        assert_eq!(observed.bytes, bytes[prefix..]);
        let receipt = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            runtime.shared.join_native_worker_receipt(&worker),
        )
        .await
        .unwrap()
        .unwrap();
        if join {
            retain_joined_helper(
                &runtime.shared,
                owner,
                lease,
                &effect,
                &observed,
                receipt.clone(),
            )
            .unwrap();
        }
        if let Some(keep_peer) = keep_peer {
            assert!(keep_peer.replace(peer).is_none());
        }
        // A true worker join alone does not assign its receipt to the Binding.
        (runtime, observed, receipt)
    }
}

#[cfg(test)]
mod joined_source_tests {
    use super::*;
    #[tokio::test]
    async fn helper_join_attachment_requires_same_runtime_pending_result_and_single_actual_worker_receipt()
     {
        let (mut engine, owner, call, lease, effect) =
            crate::network_replay::NetworkReplayEngine::controlled_pending_helper();
        let (runtime, actual, receipt) = Binding::controlled_joined_peek(
            owner,
            call,
            lease,
            &mut engine,
            b"abc",
            5,
            false,
            false,
        )
        .await;
        assert!(
            actual
                .helper_copy
                .as_ref()
                .unwrap()
                .joined_worker()
                .is_err()
        );
        let before = format!("{:?}", runtime.shared.native_streams.lock().unwrap());
        for variant in 0..6 {
            let mut observed = actual.clone();
            let mut changed_owner = owner;
            let mut changed_lease = lease;
            let mut changed_effect = effect.clone();
            match variant {
                0 => observed.helper_copy = None,
                1 => {
                    observed =
                        serde_json::from_slice(&serde_json::to_vec(&actual).unwrap()).unwrap()
                }
                2 => observed.bytes[0] ^= 1,
                3 => changed_owner.mm = owner.mm.for_exec(owner.thread),
                4 => changed_lease = serde_json::from_value(serde_json::json!(999)).unwrap(),
                5 => changed_effect = Effect::Peek { maximum: 2048 },
                _ => unreachable!(),
            }
            assert!(
                retain_joined_helper(
                    &runtime.shared,
                    changed_owner,
                    changed_lease,
                    &changed_effect,
                    &observed,
                    receipt.clone()
                )
                .is_err(),
                "variant {variant}"
            );
            assert_eq!(
                format!("{:?}", runtime.shared.native_streams.lock().unwrap()),
                before
            );
            assert!(
                actual
                    .helper_copy
                    .as_ref()
                    .unwrap()
                    .joined_worker()
                    .is_err()
            );
        }
        let (foreign, _) = super::super::tests::fixture(121);
        let (worker, reply) = foreign
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), || Ok(()))
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), reply)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let foreign_receipt = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            foreign.shared.join_native_worker_receipt(&worker),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            retain_joined_helper(
                &runtime.shared,
                owner,
                lease,
                &effect,
                &actual,
                foreign_receipt
            )
            .is_err()
        );
        assert_eq!(
            format!("{:?}", runtime.shared.native_streams.lock().unwrap()),
            before
        );
        retain_joined_helper(
            &runtime.shared,
            owner,
            lease,
            &effect,
            &actual,
            receipt.clone(),
        )
        .unwrap();
        let completion = actual.helper_copy.as_ref().unwrap();
        assert!(completion.joined_worker().unwrap().same(&receipt));
        completion
            .joined_worker()
            .unwrap()
            .validate_runtime(&runtime.shared)
            .unwrap();
        let retained = format!("{:?}", runtime.shared.native_streams.lock().unwrap());
        assert!(
            retain_joined_helper(&runtime.shared, owner, lease, &effect, &actual, receipt).is_err()
        );
        assert_eq!(
            format!("{:?}", runtime.shared.native_streams.lock().unwrap()),
            retained
        );
        // Attachment is custody only. Neither runtime nor engine is confirmed.
        runtime
            .preflight_native_stream(owner, lease, &effect, &actual)
            .unwrap();
        assert!(runtime.finish_native_stream_lease(owner, lease).is_err());
        assert!(
            engine
                .confirm_stream_physical(owner, lease, actual.confirmation.clone())
                .is_err()
        );
    }
}

#[cfg(test)]
mod supplied_runtime_source_tests {
    use super::*;
    #[tokio::test]
    async fn helper_source_uses_supplied_actual_root_runtime_owner_and_same_engine_publication() {
        let thread = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let (runtime, root, _metadata, _memory, _claim) =
            super::super::controlled_foreground_runtime(thread);
        let owner = root.owner();
        let shared = Arc::downgrade(&runtime.shared);
        let (mut engine, call, lease, effect) =
            crate::network_replay::NetworkReplayEngine::controlled_pending_helper_for_owner(owner);
        let (runtime, observed, receipt) = Binding::controlled_joined_peek_on(
            runtime,
            owner,
            call,
            lease,
            &mut engine,
            b"abc",
            5,
            true,
            false,
        )
        .await;
        assert!(shared.ptr_eq(&Arc::downgrade(&runtime.shared)));
        assert!(Arc::ptr_eq(&root, &runtime.foreground_root(owner).unwrap()));
        let engine = Arc::new(Mutex::new(engine));
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .retain_capture_publication(
                owner,
                call,
                NativeCaptureRecovery::new(
                    engine.clone(),
                    Arc::new(tokio::sync::Notify::new()),
                    |_| {},
                ),
            )
            .unwrap();
        let completion = observed.helper_copy.as_ref().unwrap();
        assert_eq!(completion.binding().owner(), owner);
        assert_eq!(completion.binding().call(), call);
        assert!(completion.joined_worker().unwrap().same(&receipt));
        completion
            .joined_worker()
            .unwrap()
            .validate_runtime(&runtime.shared)
            .unwrap();
        runtime
            .preflight_native_stream(owner, lease, &effect, &observed)
            .unwrap();
        runtime
            .confirm_native_stream(owner, lease, &effect, &observed)
            .unwrap();
        // A physical custody fixture is not an installed-origin source or a
        // semantic discharge. The old engine fence remains positively tested.
        assert!(
            engine
                .lock()
                .unwrap()
                .confirm_stream_physical(owner, lease, observed.confirmation.clone())
                .is_err()
        );
    }
}

#[cfg(test)]
impl NetworkRuntimeResources {
    /// Real same-held-socket Drain, Pending and worker join. The parser input is
    /// explicitly controlled provider geometry; no native BPF claim is made.
    pub(crate) async fn controlled_private_drain(
        &self,
        full: crate::network_replay::FullStoreCompletion,
        engine: Arc<Mutex<crate::network_replay::NetworkReplayEngine>>,
        version: u64,
        mutation: &str,
        join: bool,
    ) -> io::Result<NativeObservation> {
        let owner = full.store().owner();
        let lease = full.store().lease();
        let effect = Effect::Drain {
            maximum: full.store().length(),
        };
        let shared = self.shared.clone();
        let retained_effect = effect.clone();
        let mutation = mutation.to_owned();
        let (worker, reply) =
            self.shared
                .start_native_worker(tokio::runtime::Handle::current(), move || {
                    let work = shared.native_streams.lock().unwrap().prepare_with_store(
                        owner,
                        lease,
                        retained_effect.clone(),
                        Some(&full),
                    )?;
                    let held = work.helper.as_ref().unwrap().clone();
                    engine
                        .lock()
                        .unwrap()
                        .bind_private_drain_helper(&full, held.binding())
                        .map_err(io::Error::other)?;
                    // Explicitly consume before the observed call to test short/EOF raw
                    // retention. Geometry remains a controlled premise in these cases.
                    if mutation == "short" || mutation == "zero" {
                        let mut scratch = [0u8; 512];
                        let n = if mutation == "short" {
                            full.store().length() / 2
                        } else {
                            full.store().length()
                        };
                        assert_eq!(
                            unsafe {
                                libc::recv(
                                    work.original.as_raw_fd(),
                                    scratch.as_mut_ptr().cast(),
                                    n,
                                    libc::MSG_DONTWAIT,
                                )
                            },
                            n as isize
                        );
                    }
                    let mut observed = work.perform();
                    if mutation == "bytes" {
                        observed.bytes[0] ^= 0xff;
                        observed.confirmation = ResultValue::Drained {
                            bytes: observed.bytes.clone(),
                        };
                    }
                    let order = u64::from(mutation == "order");
                    let before = u64::from(mutation == "before");
                    let observed =
                        held.controlled_observation_at(observed, version, before, order)?;
                    shared.native_streams.lock().unwrap().retain(
                        owner,
                        lease,
                        &retained_effect,
                        observed.clone(),
                    )?;
                    Ok(observed)
                })?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), reply)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        let receipt = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            self.shared.join_native_worker_receipt(&worker),
        )
        .await
        .map_err(io::Error::other)??;
        let observed = result?;
        if join {
            retain_joined_helper(&self.shared, owner, lease, &effect, &observed, receipt)?;
        }
        Ok(observed)
    }
}
