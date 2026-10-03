//! One op26 observation on the existing shared Transmit Call. The controller
//! retains native command debt; the R SourceJobs owner retains the held read.
//! No field here can replace either owner's positive completion.
use std::io;
use std::os::fd::AsFd;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use reverie::syscalls::ArmedExecutableSource;
use reverie::syscalls::ExecutableBackingGeometry;
use reverie::syscalls::ExecutableBackingProof;
use reverie::syscalls::ExecutableCaptureRequest;
use reverie::syscalls::ExecutableSourceArmer;
use reverie::syscalls::ExecutableSourceCapture;
use reverie::syscalls::ExecutableSourceChallenge;
use reverie::syscalls::NativeUserReadError;
use reverie::syscalls::NativeUserReadRefusal;

use super::ForegroundRoot;
use super::NetworkRuntimeResources;
use super::PidfdIdentity;
use super::accepted_controller::Controller;
use super::accepted_controller::Effect as RequestKey;
use super::accepted_provider::Observation;
use super::accepted_provider::Reply;
use super::accepted_provider::Request;
use super::accepted_provider::executable_source::Effect;
use super::accepted_provider::executable_source::Intent;
use super::accepted_provider::executable_source::{self as wire};
use crate::network_replay::NetworkStreamCallId;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Phase {
    Prepared,
    Arming,
    Armed,
    Collecting,
    Collected,
    Acknowledging,
    Acked,
    Joined,
}
#[derive(Debug)]
struct State {
    phase: Phase,
    deadline: Option<Instant>,
    request: Option<ExecutableCaptureRequest>,
    intent: Option<Intent>,
    sequences: [Option<u64>; 3],
    armed: Option<Observation<u64>>,
    collected: Option<Observation<Effect>>,
    acknowledgement: Option<super::accepted_provider::CallStatus>,
    failure: Option<String>,
}
#[derive(Debug)]
pub(crate) struct ExecutableCapture {
    root: Arc<ForegroundRoot>,
    call: NetworkStreamCallId,
    address: usize,
    length: usize,
    target: OwnedFd,
    target_identity: PidfdIdentity,
    controller: Arc<Controller>,
    state: Mutex<State>,
}
impl NetworkRuntimeResources {
    /// Call outside the physical-lineage lock: duplication borrows that same
    /// registry. Global subsequently revalidates the selected grant and installs
    /// this owner on its existing Call before entering any arbitrary R callback.
    pub(crate) fn prepare_executable_capture(
        &self,
        root: Arc<ForegroundRoot>,
        call: NetworkStreamCallId,
        address: usize,
        length: usize,
    ) -> io::Result<Arc<ExecutableCapture>> {
        if !self
            .shared
            .copy_wire
            .is_some_and(|wire| wire.has_executable_source())
            || !root.is_current(root.owner())
            || !root.has_shared_mm_history()
            || address == 0
            || !(1..=512).contains(&length)
            || address
                .checked_add(length)
                .is_none_or(|end| address / 4096 != (end - 1) / 4096)
        {
            return Err(io::Error::other(
                "executable capture lacks current ABI11 shared source",
            ));
        }
        let target = self.prepare_native_capture_task(root.owner())?;
        let target_identity = PidfdIdentity::read(&target)?;
        let controller = self
            .shared
            .controller
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(|| {
                io::Error::other("executable capture requires existing independent driver")
            })?
            .as_ref()
            .map_err(|error| io::Error::other(error.clone()))?
            .clone();
        Ok(Arc::new(ExecutableCapture {
            root,
            call,
            address,
            length,
            target,
            target_identity,
            controller,
            state: Mutex::new(State {
                phase: Phase::Prepared,
                deadline: None,
                request: None,
                intent: None,
                sequences: [None; 3],
                armed: None,
                collected: None,
                acknowledgement: None,
                failure: None,
            }),
        }))
    }
}
impl ExecutableCapture {
    pub(crate) fn matches(
        &self,
        root: &Arc<ForegroundRoot>,
        call: NetworkStreamCallId,
        address: usize,
        length: usize,
    ) -> bool {
        Arc::ptr_eq(&self.root, root)
            && self.call == call
            && self.address == address
            && self.length == length
            && root.is_current(root.owner())
            && PidfdIdentity::read(&self.target).ok() == Some(self.target_identity)
    }
    pub(crate) fn armer(self: &Arc<Self>) -> Box<dyn ExecutableSourceArmer> {
        Box::new(Adapter(Arc::clone(self)))
    }
    fn failed(&self, error: impl std::fmt::Display) -> io::Error {
        let text = error.to_string();
        self.state
            .lock()
            .unwrap()
            .failure
            .get_or_insert_with(|| text.clone());
        let error = io::Error::other(text);
        self.controller.fail(&error);
        error
    }
    fn live_deadline(&self) -> io::Result<Instant> {
        let state = self.state.lock().unwrap();
        if let Some(error) = &state.failure {
            return Err(io::Error::other(error.clone()));
        }
        let deadline = state
            .deadline
            .ok_or_else(|| io::Error::other("executable capture unarmed"))?;
        drop(state);
        if Instant::now() >= deadline {
            return Err(self.failed("executable capture original housekeeping deadline expired"));
        }
        if !self.matches(&self.root, self.call, self.address, self.length) {
            return Err(self.failed("executable capture target generation changed"));
        }
        Ok(deadline)
    }
    fn arm(&self, request: ExecutableCaptureRequest) -> io::Result<()> {
        if request.target_tid != self.root.thread()
            || request.ptracer_tid <= 0
            || request.source_address != self.address
            || request.source_length != self.length
            || request.ptrace_request() != 0x4204
            || request.register_note() != 1
            || request.register_bytes() != 216
        {
            return Err(io::Error::other(
                "executable challenge changed actual target/source/capture",
            ));
        }
        let intent = Intent {
            command: 0,
            registration: self.root.association().view().registration,
            owner_mm: self.root.owner().mm.generation(),
            call: self.call.native_command_call(),
            address: self.address as u64,
            length: self.length as u64,
            iovec: request.iovec_address as u64,
            registers: request.register_buffer_address as u64,
        };
        if !intent.valid_unarmed() {
            return Err(io::Error::other("executable challenge invalid operands"));
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.phase != Phase::Prepared || state.failure.is_some() {
                return Err(io::Error::other("executable challenge repeated or failed"));
            }
            // One total infrastructure budget; it is never guest time/readiness.
            state.deadline = Some(Instant::now() + Duration::from_secs(1));
            state.phase = Phase::Arming;
            state.request = Some(request);
            state.intent = Some(intent.clone());
        }
        let deadline = self.live_deadline()?;
        let sequence = self.controller.prepare(
            RequestKey::PrepareExecutableSource(self.call),
            self.root.owner(),
            &Request::PrepareExecutableSource { intent },
            || Ok(vec![self.target.as_fd().try_clone_to_owned()?]),
        )?;
        self.state.lock().unwrap().sequences[0] = Some(sequence);
        let reply = self
            .controller
            .executable_response_blocking(sequence, deadline)?;
        let Reply::Prepared(observed) = reply else {
            return Err(io::Error::other("executable ARM reply kind"));
        };
        let mut state = self.state.lock().unwrap();
        state.armed = Some(observed.clone());
        if observed.status.operation != "ap_prepare_executable_source"
            || observed.status.returned != 0
            || observed.status.errno.is_some()
            || observed.raw == 0
        {
            return Err(io::Error::other(
                "executable ARM was not positively installed",
            ));
        }
        state.intent.as_mut().unwrap().command = observed.raw;
        state.phase = Phase::Armed;
        drop(state);
        self.live_deadline()?;
        Ok(())
    }
    fn collect(&self, capture: ExecutableSourceCapture) -> io::Result<ExecutableBackingProof> {
        let deadline = self.live_deadline()?;
        let (intent, prepared) = {
            let mut state = self.state.lock().unwrap();
            if state.phase != Phase::Armed || state.request != Some(capture.request()) {
                return Err(io::Error::other(
                    "executable collection changed one-shot capture",
                ));
            }
            state.phase = Phase::Collecting;
            (state.intent.clone().unwrap(), state.sequences[0].unwrap())
        };
        let sequence = self.controller.prepare(
            RequestKey::CollectExecutableSource(self.call),
            self.root.owner(),
            &Request::CollectExecutableSource {
                call: intent.call,
                command: intent.command,
                prepared_request: prepared,
            },
            || Ok(Vec::new()),
        )?;
        self.state.lock().unwrap().sequences[1] = Some(sequence);
        let Reply::ExecutableSource(observed) = self
            .controller
            .executable_response_blocking(sequence, deadline)?
        else {
            return Err(io::Error::other("executable collection reply kind"));
        };
        self.state.lock().unwrap().collected = Some(observed.clone());
        let (provider, task, start, _) = self.root.native_identity();
        wire::validate_collection(&observed, &intent, provider, task, start)?;
        let geometry = geometry(&observed.raw.receipt.entered, &intent)?;
        // Initial-namespace tracer identity is correlated across both BPF
        // observations by validate_collection, never equated to local gettid.
        // R's private completed capture causally joins the actual original
        // ptracer/request/iovec under its original whole hold after this ARM.
        self.live_deadline()?;
        self.state.lock().unwrap().phase = Phase::Collected;
        let retired = self.controller.prepare(
            RequestKey::RetireExecutableSource(self.call),
            self.root.owner(),
            &Request::RetireExecutableSource {
                call: intent.call,
                prepared,
                completed: sequence,
            },
            || Ok(Vec::new()),
        )?;
        {
            let mut state = self.state.lock().unwrap();
            state.sequences[2] = Some(retired);
            state.phase = Phase::Acknowledging;
        }
        let Reply::ExecutableSourceRetired(status) = self
            .controller
            .executable_response_blocking(retired, deadline)?
        else {
            return Err(io::Error::other("executable ACK reply kind"));
        };
        self.state.lock().unwrap().acknowledgement = Some(status.clone());
        if status.operation != "ap_ack_command" || status.returned != 0 || status.errno.is_some() {
            return Err(io::Error::other(
                "executable command ACK remains unresolved",
            ));
        }
        self.live_deadline()?;
        self.controller.retire_executable_source(
            self.root.owner(),
            self.call,
            [prepared, sequence, retired],
        )?;
        self.state.lock().unwrap().phase = Phase::Acked;
        // SAFETY: Only the actual authenticated service/packaged C dispatch
        // returns this correlated positive collection. C independently checks
        // both full472 observations against its retained image group_anchor,
        // original non-HSM executable deny-write/direct-Btrfs dispatch, exact
        // target's parent/original ptracer, VMA and intent; it disarms only that
        // command. We retain the raw result and exact positive sidecar+command
        // ACK. The private R capture proves genuine full PRSTATUS/XSTATE under
        // its owned hold and stable iovec allocation, not copied numeric data.
        // Geometry uses explicit kernel s_dev translation and is rechecked by R.
        Ok(unsafe { capture.certify_direct_executable(geometry) })
    }
    /// Called only after stage_followed_executable_source actually returns,
    /// which includes the registered SourceJobs worker's true join.
    pub(crate) fn joined(&self, length: usize) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.phase != Phase::Acked || state.failure.is_some() || length != self.length {
            drop(state);
            return Err(self.failed("executable source lacks exact ACK and joined bytes"));
        }
        state.phase = Phase::Joined;
        Ok(())
    }
    pub(crate) fn require_joined(&self) -> io::Result<()> {
        let state = self.state.lock().unwrap();
        if state.phase != Phase::Joined || state.failure.is_some() {
            return Err(io::Error::other(
                "executable source not physically and semantically joined",
            ));
        }
        Ok(())
    }
    pub(crate) fn source_failed(&self, error: impl std::fmt::Display) {
        let _ = self.failed(error);
    }
}
fn geometry(m: &wire::Mapping, i: &Intent) -> io::Result<ExecutableBackingGeometry> {
    let device = u32::try_from(m.device).map_err(io::Error::other)?;
    let offset = m
        .vm_pgoff
        .checked_mul(4096)
        .ok_or_else(|| io::Error::other("executable file offset overflow"))?;
    let file_position = i
        .address
        .checked_sub(m.vm_start)
        .and_then(|n| offset.checked_add(n))
        .ok_or_else(|| io::Error::other("executable source geometry overflow"))?;
    if m.vm_start == 0
        || m.vm_start >= m.vm_end
        || !m.vm_start.is_multiple_of(4096)
        || !m.vm_end.is_multiple_of(4096)
        || i.address < m.vm_start
        || i.address >= m.vm_end
        || i.length > m.vm_end - i.address
        || file_position > m.file_size
        || i.length > m.file_size - file_position
        || m.inode_number == 0
    {
        return Err(io::Error::other(
            "executable source outside exact VMA/backing",
        ));
    }
    Ok(ExecutableBackingGeometry {
        vma_start: usize::try_from(m.vm_start).map_err(io::Error::other)?,
        vma_end: usize::try_from(m.vm_end).map_err(io::Error::other)?,
        file_offset: usize::try_from(offset).map_err(io::Error::other)?,
        device_major: (device >> 20) as usize,
        device_minor: (device & ((1 << 20) - 1)) as usize,
        inode: usize::try_from(m.inode_number).map_err(io::Error::other)?,
        file_size: usize::try_from(m.file_size).map_err(io::Error::other)?,
    })
}
struct Adapter(Arc<ExecutableCapture>);
fn refused(_: &io::Error) -> NativeUserReadError {
    NativeUserReadError::Refused(NativeUserReadRefusal::TargetState(
        reverie::syscalls::Errno::EIO,
    ))
}
impl ExecutableSourceArmer for Adapter {
    fn arm(
        self: Box<Self>,
        challenge: &ExecutableSourceChallenge<'_>,
    ) -> Result<Box<dyn ArmedExecutableSource>, NativeUserReadError> {
        self.0
            .arm(challenge.request())
            .map_err(|e| refused(&self.0.failed(e)))?;
        Ok(self)
    }
}
impl ArmedExecutableSource for Adapter {
    fn collect(
        self: Box<Self>,
        capture: ExecutableSourceCapture,
    ) -> Result<ExecutableBackingProof, NativeUserReadError> {
        self.0
            .collect(capture)
            .map_err(|e| refused(&self.0.failed(e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn executable_geometry_uses_kernel_device_encoding_and_exact_extent() {
        let intent = Intent {
            command: 7,
            registration: 11,
            owner_mm: 0,
            call: 13,
            address: 0x401020,
            length: 5,
            iovec: 0x700000,
            registers: 0x700100,
        };
        let mapping = wire::controlled_collection(intent.clone())
            .raw
            .receipt
            .entered;
        let g = geometry(&mapping, &intent).unwrap();
        assert_eq!(
            (g.device_major, g.device_minor, g.file_offset),
            (8, 17, 4096)
        );
        assert_eq!(
            (g.vma_start, g.vma_end, g.inode, g.file_size),
            (0x401000, 0x402000, 53, 8192)
        );
        for case in 0..6 {
            let mut bad = mapping.clone();
            match case {
                0 => bad.device = 1 << 32,
                1 => bad.vm_pgoff = u64::MAX,
                2 => bad.vm_start = intent.address + 1,
                3 => bad.vm_end = intent.address + 1,
                4 => bad.file_size = 4096 + 32 + 4,
                5 => bad.inode_number = 0,
                _ => unreachable!(),
            }
            assert!(geometry(&bad, &intent).is_err(), "case {case}");
        }
    }
}

#[cfg(test)]
impl ExecutableCapture {
    /// Negative-only component fixture. It owns a real same-task PIDFD but
    /// supplies no C observation, R capture, backing proof or source join.
    pub(crate) fn controlled_pending(
        root: Arc<ForegroundRoot>,
        call: NetworkStreamCallId,
        address: usize,
        length: usize,
    ) -> io::Result<(Arc<Self>, OwnedFd)> {
        use std::os::fd::FromRawFd;
        // On the pinned Linux ABI PIDFD_THREAD is O_EXCL. Failure is not skipped.
        let raw =
            unsafe { libc::syscall(libc::SYS_pidfd_open, root.thread(), libc::O_EXCL) } as i32;
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let target = unsafe { OwnedFd::from_raw_fd(raw) };
        let target_identity = PidfdIdentity::read(&target)?;
        let mut pair = [-1; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let endpoint = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let peer = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        let controller = Arc::new(Controller::from_startup(
            endpoint,
            [71; 16],
            super::ProviderWireFormat::Abi11Copy5,
        )?);
        Ok((
            Arc::new(Self {
                root,
                call,
                address,
                length,
                target,
                target_identity,
                controller,
                state: Mutex::new(State {
                    phase: Phase::Prepared,
                    deadline: None,
                    request: None,
                    intent: None,
                    sequences: [None; 3],
                    armed: None,
                    collected: None,
                    acknowledgement: None,
                    failure: None,
                }),
            }),
            peer,
        ))
    }
    /// Controlled positive ACK premise ONLY. Even this state cannot satisfy
    /// the actual consumption gate and cannot issue any R backing proof.
    pub(crate) fn controlled_ack_without_join(&self) {
        let mut state = self.state.lock().unwrap();
        assert_eq!(state.phase, Phase::Prepared);
        assert!(state.request.is_none() && state.collected.is_none());
        state.phase = Phase::Acked;
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
pub(crate) mod positive_fixture;
