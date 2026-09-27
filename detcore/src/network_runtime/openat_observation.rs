//! Held Openat metadata uses the existing Call and native-worker owner.
//! Only PIDFD rights cross the provider transport; every possibly blocking
//! filesystem getattr and explicit close remains in the original worker.
use std::io;
use std::sync::Arc;

use super::accepted_controller::Controller;
use super::accepted_controller::Effect;
use super::accepted_provider::CallStatus;
use super::accepted_provider::Observation;
use super::accepted_provider::OriginalEffect;
use super::accepted_provider::OriginalSelection;
use super::accepted_provider::Reply;
use super::accepted_provider::Request;
use super::*;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::original_connect::Admission;
use crate::network_replay::original_connect::Kind;

#[derive(Clone)]
pub(super) struct State {
    worker: Option<NativeWorkerHandle>,
    custody: Arc<Mutex<Custody>>,
    reply: Arc<tokio::sync::Mutex<Completion>>,
    quarantine: Arc<NativeQuarantine>,
    joined: bool,
}
#[derive(Default)]
struct Custody {
    raw: Option<Raw>,
    held: Option<Arc<OwnedFd>>,
    task: Option<Arc<OwnedFd>>,
    finished: bool,
}
#[derive(Default)]
struct Completion {
    receive: Option<tokio::sync::oneshot::Receiver<io::Result<()>>>,
    result: Option<Result<(), String>>,
}
impl State {
    fn new() -> io::Result<Self> {
        let custody = Arc::new(Mutex::new(Custody::default()));
        let quarantine = Arc::new(NativeQuarantine::default());
        // The parked wrapper directly retains this same observation custody,
        // independently of Call removal or FnOnce capture destruction.
        quarantine.retain(custody.clone())?;
        Ok(Self {
            worker: None,
            custody,
            reply: Arc::new(tokio::sync::Mutex::new(Completion::default())),
            quarantine,
            joined: false,
        })
    }
    fn checked_after_join(&self, original: &OriginalEffect) -> io::Result<Option<Checked>> {
        if !self.joined || self.quarantine.is_possible() {
            return Err(io::Error::other(
                "Openat publication precedes positive helper join/idle",
            ));
        }
        let custody = self.custody.lock().unwrap();
        if !custody.finished || custody.held.is_some() {
            return Err(io::Error::other(
                "Openat publication precedes completed candidate release",
            ));
        }
        custody
            .raw
            .as_ref()
            .ok_or_else(|| io::Error::other("Openat worker lacks retained raw result"))?
            .checked(original)
    }
    pub(super) fn closed(&self) -> bool {
        self.joined
            && !self.quarantine.is_possible()
            && self
                .custody
                .lock()
                .unwrap()
                .raw
                .as_ref()
                .is_some_and(|raw| {
                    raw.capture.as_ref().is_none_or(|c| c.returned < 0)
                        || raw
                            .release
                            .as_ref()
                            .is_some_and(|r| r.returned == 0 && r.errno.is_none())
                })
    }
}
fn retain_progress(custody: &Arc<Mutex<Custody>>, raw: &Raw) -> io::Result<()> {
    let mut retained = custody.lock().unwrap();
    if retained.finished {
        return Err(io::Error::other(
            "Openat observation changed after worker completion",
        ));
    }
    retained.raw = Some(raw.clone());
    Ok(())
}
async fn observe_worker_reply(reply: &Arc<tokio::sync::Mutex<Completion>>) -> io::Result<()> {
    let mut retained = reply.lock().await;
    if retained.result.is_none() {
        let receive = retained
            .receive
            .as_mut()
            .ok_or_else(|| io::Error::other("Openat observation lacks retained worker reply"))?;
        // Borrow the original receiver: canceling this waiter cannot consume
        // or replace it, and no await separates a ready reply from retention.
        let result = receive
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()));
        retained.receive = None;
        retained.result = Some(result);
    }
    retained
        .result
        .as_ref()
        .unwrap()
        .clone()
        .map_err(io::Error::other)
}

#[derive(Debug, Clone)]
struct Raw {
    capture: Option<CallStatus>,
    worker: Option<super::accepted_provider::PidfdIdentity>,
    prepare_request: Option<u64>,
    complete_request: Option<u64>,
    retirement_request: Option<u64>,
    prepared: Option<(u64, Observation<u64>)>,
    actual_flags: Option<CallStatus>,
    completed: Option<(
        u64,
        Observation<OriginalSelection>,
        Option<Observation<OriginalEffect>>,
    )>,
    retirement: Option<(u64, CallStatus)>,
    transport_retired: bool,
    metadata: Option<(crate::stat::DetStat, i32, Option<i32>)>,
    resolved_path: Option<std::path::PathBuf>,
    release: Option<CallStatus>,
    error: Option<String>,
}
impl Raw {
    fn empty() -> Self {
        Self {
            capture: None,
            worker: None,
            prepare_request: None,
            complete_request: None,
            retirement_request: None,
            prepared: None,
            actual_flags: None,
            completed: None,
            retirement: None,
            transport_retired: false,
            metadata: None,
            resolved_path: None,
            release: None,
            error: None,
        }
    }
    fn checked(&self, original: &OriginalEffect) -> io::Result<Option<Checked>> {
        if let Some(error) = &self.error {
            return Err(io::Error::other(error.clone()));
        }
        let capture = self
            .capture
            .as_ref()
            .ok_or_else(|| io::Error::other("Openat lacks actual candidate outcome"))?;
        if capture.operation != "Openat pidfd_getfd" {
            return Err(io::Error::other("Openat capture changed operation"));
        }
        if capture.returned < 0 {
            if capture.errno == Some(libc::EBADF)
                && self.worker.is_none()
                && self.prepare_request.is_none()
                && self.complete_request.is_none()
                && self.retirement_request.is_none()
                && self.prepared.is_none()
                && self.actual_flags.is_none()
                && self.completed.is_none()
                && self.retirement.is_none()
                && !self.transport_retired
                && self.metadata.is_none()
                && self.resolved_path.is_none()
                && self.release.is_none()
            {
                return Ok(None);
            }
            return Err(io::Error::other("Openat candidate acquisition failed"));
        }
        if self.worker.is_none()
            || self.prepare_request != self.prepared.as_ref().map(|(id, _)| *id)
            || self.complete_request != self.completed.as_ref().map(|(id, _, _)| *id)
            || self.retirement_request != self.retirement.as_ref().map(|(id, _)| *id)
            || capture.errno.is_some()
            || self.release.as_ref().is_none_or(|s| {
                s.operation != "close Openat candidate" || s.returned != 0 || s.errno.is_some()
            })
            || !self.transport_retired
        {
            return Err(io::Error::other(
                "Openat candidate/auxiliary retirement remains owned",
            ));
        }
        let (_, selected, Some(effect)) = self
            .completed
            .as_ref()
            .ok_or_else(|| io::Error::other("Openat auxiliary completion absent"))?
        else {
            return Err(io::Error::other("Openat auxiliary selection unresolved"));
        };
        let actual = self
            .actual_flags
            .as_ref()
            .ok_or_else(|| io::Error::other("Openat actual F_GETFL absent"))?;
        if selected.status.returned != 0
            || selected.status.errno.is_some()
            || effect.status.returned != 0
            || effect.status.errno.is_some()
            || effect.raw.command.operation != 23
            || selected.raw != effect.raw.original.selection
            || actual.operation != "Openat held F_GETFL"
            || actual.returned < 0
            || actual.errno.is_some()
            || effect.raw.original.returned != actual.returned
            || selected.raw.provider != original.original.selection.provider
        {
            return Err(io::Error::other(
                "Openat actual F_GETFL disagrees with retained provider result",
            ));
        }
        if selected.raw.file != original.original.selection.file {
            if self.metadata.is_none() && self.resolved_path.is_none() {
                return Ok(None);
            }
            return Err(io::Error::other(
                "different held file supplied Openat metadata",
            ));
        }
        let (stat, status_flags, domain) = self
            .metadata
            .ok_or_else(|| io::Error::other("exact Openat held file lacks real metadata"))?;
        if status_flags != actual.returned {
            return Err(io::Error::other(
                "Openat metadata getter changed observed status flags",
            ));
        }
        Ok(Some(Checked {
            provider: selected.raw.provider,
            file: selected.raw.file,
            stat,
            status_flags,
            domain,
            resolved_path: self.resolved_path.clone(),
        }))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Checked {
    pub provider: u64,
    pub file: u64,
    pub stat: crate::stat::DetStat,
    pub status_flags: i32,
    pub domain: Option<i32>,
    pub resolved_path: Option<std::path::PathBuf>,
}
impl Checked {
    pub(crate) fn kind(&self) -> io::Result<crate::fd::FdType> {
        crate::fd::FdType::from_initial_profile(
            self.stat.mode,
            self.status_flags as u32,
            libc::major(self.stat.rdev) as u32,
            libc::minor(self.stat.rdev) as u32,
        )
        .ok_or_else(|| io::Error::other("Openat held file class is unrepresented"))
    }
}
fn status(operation: &str, returned: i32) -> CallStatus {
    CallStatus {
        operation: operation.into(),
        returned,
        errno: (returned < 0).then(|| {
            io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO)
        }),
    }
}
fn namespace_identity(fd: BorrowedFd<'_>) -> io::Result<(u64, u64)> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_dev, stat.st_ino))
}
fn local_user_namespace() -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata("/proc/thread-self/ns/user")?;
    Ok((metadata.dev(), metadata.ino()))
}
/// Installed Linux PIDFD_GET_USER_NAMESPACE returns a namespace FD from this
/// retained task, with its ordinary access check. No numeric PID lookup or
/// /proc race can substitute a recycled task. Unknown support is a capability
/// error before the original syscall, never a manufactured guest errno.
pub(super) fn same_user_namespace(task: BorrowedFd<'_>) -> io::Result<(u64, u64)> {
    const PIDFD_GET_USER_NAMESPACE: libc::c_ulong = 0xff09;
    let raw = unsafe { libc::ioctl(task.as_raw_fd(), PIDFD_GET_USER_NAMESPACE, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let namespace = unsafe { OwnedFd::from_raw_fd(raw) };
    let observed = namespace_identity(namespace.as_fd());
    let closed = unsafe { libc::close(namespace.into_raw_fd()) };
    let close_error = (closed != 0).then(io::Error::last_os_error);
    let observed = observed?;
    if let Some(error) = close_error {
        return Err(error);
    }
    if observed != local_user_namespace()? {
        return Err(io::Error::other(
            "Openat metadata worker has a different user namespace",
        ));
    }
    Ok(observed)
}

fn observe_held(
    controller: &Controller,
    executor: &tokio::runtime::Handle,
    owner: NetworkStreamOwner,
    admission: &Admission,
    original: &OriginalEffect,
    held: BorrowedFd<'_>,
    raw: &mut Raw,
    custody: &Arc<Mutex<Custody>>,
    quarantine: &NativeQuarantine,
) -> io::Result<()> {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    // PIDFD_THREAD is O_EXCL in the installed pidfd UAPI. The exact retained
    // self handle, not this numeric TID, is transported and used thereafter.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tid, libc::O_EXCL) };
    if pidfd < 0 {
        return Err(io::Error::last_os_error());
    }
    let task = Arc::new(unsafe { OwnedFd::from_raw_fd(pidfd as i32) });
    raw.worker = Some(super::accepted_provider::PidfdIdentity::read(&task)?);
    custody.lock().unwrap().task = Some(task.clone());
    retain_progress(custody, raw)?;
    let call = admission.call;
    quarantine.mark_possible()?;
    let prepared = controller.prepare(
        Effect::PrepareOriginalFileObservation(call),
        owner,
        &Request::PrepareOriginalFileObservation {
            call: call.native_command_call(),
            mm: owner.mm.generation(),
            fd: held.as_raw_fd(),
            role: super::accepted_provider::AuxiliaryRole::File,
        },
        || Ok(vec![task.as_fd().try_clone_to_owned()?]),
    )?;
    raw.prepare_request = Some(prepared);
    retain_progress(custody, raw)?;
    let Reply::Prepared(observed) = executor.block_on(controller.response(prepared))? else {
        return Err(io::Error::other(
            "Openat auxiliary preparation changed reply",
        ));
    };
    raw.prepared = Some((prepared, observed.clone()));
    retain_progress(custody, raw)?;
    if observed.status.returned != 0 || observed.status.errno.is_some() || observed.raw == 0 {
        return Err(io::Error::other("Openat auxiliary preparation failed"));
    }
    let returned = unsafe { libc::fcntl(held.as_raw_fd(), libc::F_GETFL) };
    raw.actual_flags = Some(status("Openat held F_GETFL", returned));
    retain_progress(custody, raw)?;
    let completed = controller.prepare(
        Effect::CollectOriginalFileObservation(call),
        owner,
        &Request::CollectOriginalFileObservation {
            call: call.native_command_call(),
            command: observed.raw,
            prepared_request: prepared,
            role: super::accepted_provider::AuxiliaryRole::File,
        },
        || Ok(vec![]),
    )?;
    raw.complete_request = Some(completed);
    retain_progress(custody, raw)?;
    let Reply::OriginalFileObservation { selection, effect } =
        executor.block_on(controller.response(completed))?
    else {
        return Err(io::Error::other(
            "Openat auxiliary completion changed reply",
        ));
    };
    raw.completed = Some((completed, selection.clone(), effect.clone()));
    retain_progress(custody, raw)?;
    let retired = controller.prepare(
        Effect::RetireOriginalFileObservation(call),
        owner,
        &Request::RetireOriginalFileObservation {
            call: call.native_command_call(),
            prepared,
            completed,
        },
        || Ok(vec![]),
    )?;
    raw.retirement_request = Some(retired);
    retain_progress(custody, raw)?;
    let Reply::OriginalFileObservationRetired(retirement) =
        executor.block_on(controller.response(retired))?
    else {
        return Err(io::Error::other(
            "Openat auxiliary retirement changed reply",
        ));
    };
    raw.retirement = Some((retired, retirement.clone()));
    retain_progress(custody, raw)?;
    if retirement.returned != 0 || retirement.errno.is_some() {
        return Err(io::Error::other(
            "Openat auxiliary task registration remains owned",
        ));
    }
    controller.retire_file_observation(owner, call, [prepared, completed, retired])?;
    raw.transport_retired = true;
    retain_progress(custody, raw)?;
    quarantine.retired();
    // All task registration and command ownership is now retired before any
    // unrelated getter/close on this pooled worker can reach these hooks.
    if returned < 0
        || selection.status.returned != 0
        || selection.status.errno.is_some()
        || effect.as_ref().is_none_or(|effect| {
            effect.status.returned != 0
                || effect.status.errno.is_some()
                || effect.raw.original.returned != returned
                || effect.raw.original.selection != selection.raw
        })
        || selection.raw.provider != original.original.selection.provider
    {
        return Err(io::Error::other(
            "Openat held F_GETFL lacks matching actual result",
        ));
    }
    if selection.raw.file == original.original.selection.file {
        raw.metadata = Some(installation_observation::held_file_profile(held)?);
        // This is only a fallback annotation from the authenticated held file.
        // It is never a second numeric guest-FD identity certificate.
        raw.resolved_path =
            std::fs::read_link(format!("/proc/thread-self/fd/{}", held.as_raw_fd()))
                .ok()
                .filter(|path| path.is_absolute());
    }
    Ok(())
}

impl NetworkRuntimeResources {
    /// A synchronous read of the same completed observation. It cannot submit
    /// work, duplicate an FD, join a worker, or turn a pending receipt into None.
    pub(crate) fn original_openat_observed(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> io::Result<Option<Checked>> {
        self.shared.original_openat_observed(owner, admission)
    }
    pub(crate) async fn observe_original_openat(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> io::Result<Option<Checked>> {
        self.shared
            .observe_original_openat(owner, admission, None)
            .await
    }
}
impl RuntimeShared {
    fn original_openat_observed(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> io::Result<Option<Checked>> {
        let mut calls = self.native_streams.lock().unwrap();
        let state = calls.original(owner, admission.call)?;
        if state.admission != *admission
            || admission.arguments.kind != Kind::Openat
            || !state.retired
            || !state.close_queued
        {
            return Err(io::Error::other(
                "Openat observation lacks completed original custody",
            ));
        }
        let original = state.completed_allocator_effect()?;
        if original.original.returned < 0 {
            if state.openat_observation.is_some() {
                return Err(io::Error::other(
                    "failed Openat unexpectedly owns an auxiliary observation",
                ));
            }
            return Ok(None);
        }
        state
            .openat_observation
            .as_ref()
            .ok_or_else(|| io::Error::other("Openat publication precedes held-file observation"))?
            .checked_after_join(&original)
    }

    pub(super) async fn observe_original_openat(
        self: &Arc<Self>,
        owner: NetworkStreamOwner,
        admission: &Admission,
        successor: Option<NetworkStreamOwner>,
    ) -> io::Result<Option<Checked>> {
        let controller = self
            .controller
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| io::Error::other("Openat lacks provider controller"))?
            .map_err(io::Error::other)?;
        let successor_task = if let Some(publisher) = successor {
            let (bound, actual) = {
                let mut calls = self.native_streams.lock().unwrap();
                let state = calls.original(owner, admission.call)?;
                let bound = state
                    .installation_owner
                    .clone()
                    .ok_or_else(|| io::Error::other("terminal Openat lacks original owner"))?;
                (bound.clone(), bound.metadata.clone())
            };
            let metadata = actual.lock().unwrap();
            let publication = self
                .native_streams
                .lock()
                .unwrap()
                .original(owner, admission.call)?
                .publication
                .clone();
            publication
                .engine
                .lock()
                .unwrap()
                .validate_fd_metadata(publisher, bound.files, &actual, &metadata)
                .map_err(io::Error::other)?;
            let physical = self.physical.lock().unwrap();
            let (provider, _, _, table) = physical.installation_identity(publisher)?;
            if (provider, table) != (bound.provider, bound.table) {
                return Err(io::Error::other(
                    "terminal Openat successor changed physical table",
                ));
            }
            let target = Arc::new(physical.get(publisher)?.as_fd().try_clone_to_owned()?);
            drop(physical);
            let namespace = same_user_namespace(target.as_fd())?;
            Some((target, namespace))
        } else {
            None
        };
        let (start, original, target, namespace, executor) = {
            let mut calls = self.native_streams.lock().unwrap();
            let state = calls.original(owner, admission.call)?;
            if state.admission != *admission
                || admission.arguments.kind != Kind::Openat
                || !state.retired
                || !state.close_queued
            {
                return Err(io::Error::other(
                    "Openat observation lacks completed original custody",
                ));
            }
            let original = state.completed_allocator_effect()?;
            if original.original.returned < 0 {
                return Ok(None);
            }
            let (target, namespace) = match &successor_task {
                Some((target, namespace)) => (target.clone(), *namespace),
                None => (
                    state
                        .allocation_task
                        .clone()
                        .ok_or_else(|| io::Error::other("Openat lost original task PIDFD"))?,
                    state.allocation_userns.ok_or_else(|| {
                        io::Error::other("Openat lacks original user namespace proof")
                    })?,
                ),
            };
            let start = state.openat_observation.is_none();
            if start {
                state.openat_observation = Some(State::new()?);
            }
            (start, original, target, namespace, state.executor.clone())
        };
        if start {
            let admitted = admission.clone();
            let effect = original.clone();
            let worker_executor = executor.clone();
            let observation = self
                .native_streams
                .lock()
                .unwrap()
                .original(owner, admission.call)?
                .openat_observation
                .as_ref()
                .unwrap()
                .clone();
            let custody = observation.custody.clone();
            let worker_quarantine = observation.quarantine.clone();
            let operation = move || {
                let mut raw = Raw::empty();
                let result =
                    (|| {
                        if local_user_namespace()? != namespace {
                            return Err(io::Error::other(
                                "Openat worker namespace changed after original entry",
                            ));
                        }
                        let captured = unsafe {
                            libc::syscall(
                                libc::SYS_pidfd_getfd,
                                target.as_raw_fd(),
                                effect.original.returned,
                                0,
                            )
                        } as i32;
                        raw.capture = Some(status("Openat pidfd_getfd", captured));
                        // Own the actual returned descriptor before any fallible
                        // lock/retention path can leave a bare acquired integer.
                        let held = (captured >= 0)
                            .then(|| Arc::new(unsafe { OwnedFd::from_raw_fd(captured) }));
                        if let Some(held) = &held {
                            custody.lock().unwrap().held = Some(held.clone());
                        }
                        retain_progress(&custody, &raw)?;
                        let Some(held) = held else {
                            return Ok(());
                        };
                        let outcome = observe_held(
                            &controller,
                            &executor,
                            owner,
                            &admitted,
                            &effect,
                            held.as_fd(),
                            &mut raw,
                            &custody,
                            &worker_quarantine,
                        );
                        drop(held);
                        if worker_quarantine.is_possible() {
                            // The exact candidate/PIDFD/raw history belongs to the
                            // quarantined wrapper through controller process exit.
                            // A close here would release uncertain command custody.
                            return Err(outcome.err().unwrap_or_else(|| io::Error::other(
                            "Openat helper cannot be idle while registration remains possible")));
                        }
                        let held =
                            custody.lock().unwrap().held.take().ok_or_else(|| {
                                io::Error::other("Openat candidate custody vanished")
                            })?;
                        let held = match Arc::try_unwrap(held) {
                            Ok(held) => held,
                            Err(held) => {
                                custody.lock().unwrap().held = Some(held);
                                return Err(io::Error::other(
                                    "Openat candidate remains borrowed at explicit close",
                                ));
                            }
                        };
                        // Exactly one actual close after registration is known
                        // idle (or before any registration could be submitted).
                        let closed = unsafe { libc::close(held.into_raw_fd()) };
                        raw.release = Some(status("close Openat candidate", closed));
                        retain_progress(&custody, &raw)?;
                        outcome?;
                        if closed != 0 {
                            return Err(io::Error::other("Openat candidate close failed"));
                        }
                        Ok(())
                    })();
                raw.error = result.as_ref().err().map(ToString::to_string);
                retain_progress(&custody, &raw)?;
                custody.lock().unwrap().finished = true;
                // The original completion channel delivers this error before
                // the same worker parks; raw facts are already durable here.
                result
            };
            let retiring = successor.map(|_| NativeRetirement::Original(owner, admission.call));
            let submitted = self.start_native_worker_with_quarantine(
                worker_executor,
                retiring,
                false,
                Some(observation.quarantine.clone()),
                operation,
            );
            match submitted {
                Ok((worker, receive)) => {
                    let mut calls = self.native_streams.lock().unwrap();
                    calls
                        .original(owner, admission.call)?
                        .openat_observation
                        .as_mut()
                        .unwrap()
                        .worker = Some(worker);
                    // The same Call retains the receiver across canceled
                    // waiters; error delivery must not await a parked JoinHandle.
                    *observation
                        .reply
                        .try_lock()
                        .expect("new Openat reply has no waiter") = Completion {
                        receive: Some(receive),
                        result: None,
                    };
                }
                Err(error) => {
                    let mut raw = Raw::empty();
                    raw.error = Some(error.to_string());
                    let mut calls = self.native_streams.lock().unwrap();
                    let state = calls
                        .original(owner, admission.call)?
                        .openat_observation
                        .as_mut()
                        .unwrap();
                    state.custody.lock().unwrap().raw = Some(raw);
                    state.joined = true;
                    return Err(error);
                }
            }
        }
        let observation = self
            .native_streams
            .lock()
            .unwrap()
            .original(owner, admission.call)?
            .openat_observation
            .as_ref()
            .unwrap()
            .clone();
        let worker = observation
            .worker
            .clone()
            .ok_or_else(|| io::Error::other("Openat observation lacks its submitted worker"))?;
        let completed = observe_worker_reply(&observation.reply).await;
        if let Err(error) = completed {
            // Includes the wrapper's panic path, whose last raw prefix remains
            // in the original custody even when the operation did not return.
            let mut custody = observation
                .custody
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            custody
                .raw
                .get_or_insert_with(Raw::empty)
                .error
                .get_or_insert_with(|| error.to_string());
            return Err(error);
        }
        if observation.quarantine.is_possible() {
            return Err(io::Error::other(
                "Openat possible registration cannot certify idle worker",
            ));
        }
        // A successful callback still needs the original JoinHandle and its
        // deadline. The failure path above cannot block on quarantine instead.
        self.join_native_worker(&worker).await?;
        let mut calls = self.native_streams.lock().unwrap();
        let state = calls
            .original(owner, admission.call)?
            .openat_observation
            .as_mut()
            .unwrap();
        state.joined = true;
        state.checked_after_join(&original)
    }
}

#[cfg(test)]
mod quarantine_tests {
    use super::*;

    #[tokio::test]
    async fn openat_worker_error_survives_waiter_cancellation_without_join_or_idle_claim() {
        let state = State::new().unwrap();
        let (send, receive) = tokio::sync::oneshot::channel();
        state.reply.lock().await.receive = Some(receive);
        let mut canceled = Box::pin(observe_worker_reply(&state.reply));
        assert!(futures::poll!(canceled.as_mut()).is_pending());
        drop(canceled);
        send.send(Err(io::Error::other(
            "component Openat registration unknown",
        )))
        .unwrap();
        for _ in 0..2 {
            let error = observe_worker_reply(&state.reply).await.unwrap_err();
            assert_eq!(error.to_string(), "component Openat registration unknown");
        }
        assert!(state.reply.lock().await.receive.is_none());
        assert!(!state.joined);
        assert!(!state.closed());
    }

    #[test]
    fn openat_possible_registration_retains_candidate_task_and_raw_prefix_after_call_drop() {
        let state = State::new().unwrap();
        let candidate = Arc::new(OwnedFd::from(std::fs::File::open("/dev/null").unwrap()));
        let fd = candidate.as_raw_fd();
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        let raw_pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tid, libc::O_EXCL) };
        assert!(raw_pidfd >= 0);
        let task = Arc::new(unsafe { OwnedFd::from_raw_fd(raw_pidfd as i32) });
        let weak_task = Arc::downgrade(&task);
        let mut raw = Raw::empty();
        raw.capture = Some(CallStatus {
            operation: "Openat pidfd_getfd".into(),
            returned: fd,
            errno: None,
        });
        raw.worker = Some(super::super::accepted_provider::PidfdIdentity::read(&task).unwrap());
        raw.prepare_request = Some(7); // explicit component unknown-outcome premise
        {
            let mut custody = state.custody.lock().unwrap();
            custody.held = Some(candidate.clone());
            custody.task = Some(task.clone());
        }
        retain_progress(&state.custody, &raw).unwrap();
        state.quarantine.mark_possible().unwrap();
        let quarantine = state.quarantine.clone(); // exact wrapper-owned capability
        let custody = Arc::downgrade(&state.custody);
        assert!(!state.closed());
        drop(task);
        drop(candidate);
        drop(state); // Call and callback ownership gone
        let custody = custody
            .upgrade()
            .expect("wrapper must retain original Openat custody");
        let retained = custody.lock().unwrap();
        assert_eq!(retained.raw.as_ref().unwrap().prepare_request, Some(7));
        assert!(retained.raw.as_ref().unwrap().prepared.is_none());
        assert!(retained.raw.as_ref().unwrap().release.is_none());
        assert!(!retained.raw.as_ref().unwrap().transport_retired);
        assert!(retained.held.is_some());
        assert!(weak_task.upgrade().is_some());
        assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
        assert!(quarantine.is_possible());
    }

    #[test]
    fn openat_progress_cannot_overwrite_final_retained_native_outcome() {
        let custody = Arc::new(Mutex::new(Custody::default()));
        let mut raw = Raw::empty();
        raw.capture = Some(CallStatus {
            operation: "Openat pidfd_getfd".into(),
            returned: -1,
            errno: Some(libc::EBADF),
        });
        retain_progress(&custody, &raw).unwrap();
        custody.lock().unwrap().finished = true;
        raw.capture.as_mut().unwrap().errno = Some(libc::EPERM);
        assert!(retain_progress(&custody, &raw).is_err());
        assert_eq!(
            custody
                .lock()
                .unwrap()
                .raw
                .as_ref()
                .unwrap()
                .capture
                .as_ref()
                .unwrap()
                .errno,
            Some(libc::EBADF)
        );
    }
}

#[cfg(test)]
mod publication_gate_tests {
    use super::*;
    use crate::network_replay::original_connect::Arguments;
    use crate::resources::ExternalOpId;
    use crate::types::DetTid;
    use crate::types::FilesId;
    use crate::types::MmId;

    // Explicit upstream-completion premises for the real same-Call accessor.
    // These component inputs are not native collection/ACK qualification.
    fn fixture(returned: i32) -> (NetworkRuntimeResources, NetworkStreamOwner, Admission) {
        let (runtime, _) = super::super::tests::fixture(83);
        let thread = DetTid::from_raw(83);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let admission = Admission {
            call: crate::network_replay::NetworkStreamCallId::controlled_fixture(17),
            arguments: Arguments {
                kind: Kind::Openat,
                operation: ExternalOpId::new(thread, 19),
                files: FilesId::initial(thread),
                binding: None,
                fd: libc::AT_FDCWD,
                address: 0x1234,
                length: libc::O_RDONLY | libc::O_CLOEXEC,
                original_count: 0,
            },
        };
        let engine = crate::network_replay::NetworkReplayEngine::record(
            chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
        );
        let publication = NativeCaptureRecovery::new(
            Arc::new(Mutex::new(engine)),
            Arc::new(tokio::sync::Notify::new()),
            |_| {},
        );
        let mut calls = runtime.shared.native_streams.lock().unwrap();
        calls
            .capture_original(
                owner,
                admission.clone(),
                None,
                tokio::runtime::Handle::current(),
                publication,
            )
            .unwrap();
        let state = calls.original(owner, admission.call).unwrap();
        let mut raw = super::super::accepted_provider_ffi::OriginalEffect::default();
        raw.command.operation = 18;
        raw.command.command = 7;
        raw.original.selection.provider = 83;
        raw.original.selection.file = 91;
        raw.original.complete = 1;
        raw.original.returned = returned;
        state.completion = Some(raw.into());
        state.retired = true;
        state.close_queued = true;
        drop(calls);
        (runtime, owner, admission)
    }

    fn completed_observation(original: &OriginalEffect) -> State {
        let mut observation = State::new().unwrap();
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        let raw_pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tid, libc::O_EXCL) };
        assert!(raw_pidfd >= 0);
        let task = Arc::new(unsafe { OwnedFd::from_raw_fd(raw_pidfd as i32) });
        let success = |operation: &str, returned| CallStatus {
            operation: operation.into(),
            returned,
            errno: None,
        };
        let mut effect = original.clone();
        effect.command.operation = 23;
        effect.original.returned = libc::O_RDONLY;
        let selection = effect.original.selection.clone();
        let raw = Raw {
            capture: Some(success("Openat pidfd_getfd", 7)),
            worker: Some(super::super::accepted_provider::PidfdIdentity::read(&task).unwrap()),
            prepare_request: Some(11),
            complete_request: Some(12),
            retirement_request: Some(13),
            prepared: Some((
                11,
                Observation {
                    status: success("prepare", 0),
                    raw: 7,
                },
            )),
            actual_flags: Some(success("Openat held F_GETFL", libc::O_RDONLY)),
            completed: Some((
                12,
                Observation {
                    status: success("selected", 0),
                    raw: selection,
                },
                Some(Observation {
                    status: success("effect", 0),
                    raw: effect,
                }),
            )),
            retirement: Some((13, success("retire", 0))),
            transport_retired: true,
            metadata: Some((
                crate::stat::DetStat {
                    mode: libc::S_IFREG,
                    ..Default::default()
                },
                libc::O_RDONLY,
                None,
            )),
            resolved_path: Some("/component/retained".into()),
            release: Some(success("close Openat candidate", 0)),
            error: None,
        };
        {
            let mut custody = observation.custody.lock().unwrap();
            custody.raw = Some(raw);
            custody.task = Some(task);
            custody.finished = true;
        }
        observation.joined = true;
        observation
    }

    #[tokio::test]
    async fn openat_cached_publication_requires_completed_same_call_and_preserves_negative() {
        let (runtime, owner, admission) = fixture(-libc::ENOENT);
        assert!(
            runtime
                .original_openat_observed(owner, &admission)
                .unwrap()
                .is_none()
        );
        assert!(
            runtime
                .shared
                .native_streams
                .lock()
                .unwrap()
                .original(owner, admission.call)
                .unwrap()
                .openat_observation
                .is_none()
        );
        for mutation in 0..4 {
            let mut changed = admission.clone();
            let mut changed_owner = owner;
            match mutation {
                0 => changed.arguments.address += 1,
                1 => changed.arguments.kind = Kind::EpollCtl,
                2 => {
                    changed.call =
                        crate::network_replay::NetworkStreamCallId::controlled_fixture(18)
                }
                _ => changed_owner.mm = owner.mm.for_exec(owner.thread),
            }
            assert!(
                runtime
                    .original_openat_observed(changed_owner, &changed)
                    .is_err()
            );
        }
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .original(owner, admission.call)
            .unwrap()
            .openat_observation = Some(State::new().unwrap());
        assert!(runtime.original_openat_observed(owner, &admission).is_err());
        let (runtime, owner, admission) = fixture(7);
        assert!(
            runtime
                .original_openat_observed(owner, &admission)
                .unwrap_err()
                .to_string()
                .contains("precedes held-file observation")
        );
    }

    #[tokio::test]
    async fn openat_cached_publication_reentry_never_restarts_or_consumes_helper() {
        let (runtime, owner, admission) = fixture(7);
        let retained = {
            let mut calls = runtime.shared.native_streams.lock().unwrap();
            let state = calls.original(owner, admission.call).unwrap();
            let observation = completed_observation(&state.completed_allocator_effect().unwrap());
            let retained = observation.custody.clone();
            state.openat_observation = Some(observation);
            retained
        };
        let raw_before = format!("{:?}", retained.lock().unwrap().raw);
        for _ in 0..3 {
            let checked = runtime
                .original_openat_observed(owner, &admission)
                .unwrap()
                .unwrap();
            assert_eq!((checked.provider, checked.file), (83, 91));
            assert_eq!(checked.resolved_path, Some("/component/retained".into()));
            let mut calls = runtime.shared.native_streams.lock().unwrap();
            let observed = calls
                .original(owner, admission.call)
                .unwrap()
                .openat_observation
                .as_ref()
                .unwrap();
            assert!(Arc::ptr_eq(&observed.custody, &retained));
            assert!(
                observed.worker.is_none(),
                "accessor must not submit a worker"
            );
            assert!(observed.reply.try_lock().unwrap().receive.is_none());
        }
        assert_eq!(format!("{:?}", retained.lock().unwrap().raw), raw_before);
    }

    #[tokio::test]
    async fn openat_cached_publication_refuses_helper_unknown_error_and_unreleased_candidate() {
        for mutation in 0..7 {
            let (runtime, owner, admission) = fixture(7);
            let mut calls = runtime.shared.native_streams.lock().unwrap();
            let state = calls.original(owner, admission.call).unwrap();
            let mut observation =
                completed_observation(&state.completed_allocator_effect().unwrap());
            match mutation {
                0 => observation.joined = false,
                1 => observation.quarantine.mark_possible().unwrap(),
                2 => observation.custody.lock().unwrap().finished = false,
                3 => {
                    observation.custody.lock().unwrap().held =
                        Some(Arc::new(std::fs::File::open("/dev/null").unwrap().into()))
                }
                4 => {
                    observation
                        .custody
                        .lock()
                        .unwrap()
                        .raw
                        .as_mut()
                        .unwrap()
                        .error = Some("actual retained helper failure".into())
                }
                5 => {
                    observation
                        .custody
                        .lock()
                        .unwrap()
                        .raw
                        .as_mut()
                        .unwrap()
                        .transport_retired = false
                }
                _ => {
                    observation
                        .custody
                        .lock()
                        .unwrap()
                        .raw
                        .as_mut()
                        .unwrap()
                        .release = None
                }
            }
            state.openat_observation = Some(observation);
            drop(calls);
            let error = runtime
                .original_openat_observed(owner, &admission)
                .unwrap_err();
            if mutation == 4 {
                assert_eq!(error.to_string(), "actual retained helper failure");
            }
            assert!(
                runtime
                    .shared
                    .native_streams
                    .lock()
                    .unwrap()
                    .original(owner, admission.call)
                    .unwrap()
                    .openat_observation
                    .is_some(),
                "refusal must retain the same custody"
            );
        }
    }

    #[tokio::test]
    async fn openat_cancelled_helper_wait_cannot_authorize_cached_publication() {
        let (runtime, owner, admission) = fixture(7);
        let state = State::new().unwrap();
        let (send, receive) = tokio::sync::oneshot::channel();
        state.reply.lock().await.receive = Some(receive);
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .original(owner, admission.call)
            .unwrap()
            .openat_observation = Some(state.clone());
        let mut canceled = Box::pin(observe_worker_reply(&state.reply));
        assert!(futures::poll!(canceled.as_mut()).is_pending());
        drop(canceled);
        assert!(runtime.original_openat_observed(owner, &admission).is_err());
        send.send(Err(io::Error::other("retained observation failure")))
            .unwrap();
        assert_eq!(
            observe_worker_reply(&state.reply)
                .await
                .unwrap_err()
                .to_string(),
            "retained observation failure"
        );
        assert!(runtime.original_openat_observed(owner, &admission).is_err());
        assert!(!state.joined && !state.closed());
        assert!(state.reply.lock().await.receive.is_none());
        assert_eq!(
            state
                .reply
                .lock()
                .await
                .result
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err(),
            "retained observation failure"
        );
    }
}
