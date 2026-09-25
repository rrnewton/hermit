//! Owned, non-serialized runtime handoff into the inline GlobalTool initializer.
//!
//! This carries the private startup channel, never a host descriptor number in
//! Config. The container parent/service and controller each own their endpoint.

use std::cell::RefCell;
use std::future::Future;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::sync::Mutex;

pub(crate) mod accepted;
mod accepted_controller;
pub(crate) mod accepted_creation;
mod accepted_driver;
pub(crate) mod accepted_listener;
mod accepted_parent;
mod accepted_provider;
mod accepted_provider_ffi;
mod accepted_service;
mod accepted_transport;
pub mod capability_unit;
pub mod guard;
mod grouped_broker;
mod parent;
mod physical;
#[path = "network_runtime/release/module.rs"]
mod release;
pub use accepted_parent::AcceptedProviderLaunch;
pub use accepted_parent::ParentAcceptedService;
pub use accepted_parent::ParentAcceptedStartFailure;
pub use accepted_parent::ProviderArtifact;
pub use accepted_service::run_accepted_provider_process;
pub use parent::ParentNetworkDrainPending;
pub use parent::ParentNetworkDrained;
pub use parent::ParentNetworkService;
pub use parent::ParentNetworkStartFailure;

/// One run's controller endpoint, delivered by the authenticated startup path.
/// It is deliberately neither Clone nor Serialize; a copied integer cannot
/// reconstruct this ownership capability.
#[derive(Debug)]
pub struct NetworkRuntimeResources {
    shared: std::sync::Arc<RuntimeShared>,
    #[cfg(test)]
    drops: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

/// Controller-side recovery owner, created after container clone. It is kept
/// outside the backend future; cancellation does not transfer ownership to Drop.
/// This same-process Arc is never copied into a child by the container clone.
#[derive(Debug)]
#[must_use = "keep runtime ownership through actual controller terminal cleanup"]
pub struct NetworkRuntimeOwner {
    shared: std::sync::Arc<RuntimeShared>,
}

#[derive(Debug)]
struct RuntimeShared {
    guard: Mutex<Option<GuardRuntime>>,
    endpoint: Option<OwnedFd>,
    controller: Mutex<Option<Result<std::sync::Arc<accepted_controller::Controller>, String>>>,
    driver: Mutex<Option<Result<accepted_driver::Driver, String>>>,
    transport_terminal_deadline: Mutex<Option<std::time::Instant>>,
    incarnation: [u8; 16],
    physical: Mutex<physical::CustodyTasks<OwnedFd>>,
    accepted: Mutex<accepted::AcceptedCustody<OwnedFd>>,
    listeners: Mutex<accepted_listener::Listeners<OwnedFd>>,
    creations: tokio::sync::Mutex<accepted_creation::Creations>,
}

#[derive(Debug)]
struct GuardRuntime {
    control: std::sync::Arc<dyn guard::NetworkGuardControl>,
    deadline: std::time::Instant,
    observer: Option<(guard::NetworkGuardPublication, Result<(), String>)>,
    initial: Option<(
        crate::network_replay::NetworkStreamOwner,
        Result<(), String>,
    )>,
}

impl RuntimeShared {
    fn retain_completed_collections(
        &self,
        controller: &accepted_controller::Controller,
    ) -> std::io::Result<()> {
        let pending = self.accepted.lock().unwrap().pending_collections();
        for (owner, lease, sequence) in pending {
            let reply = match controller.retained_response(sequence) {
                Ok(None) => continue,
                Ok(Some(reply)) => Ok(reply),
                Err(error) => Err(error.to_string()),
            };
            self.accepted
                .lock()
                .unwrap()
                .complete_collection(owner, lease, sequence, reply)?;
        }
        Ok(())
    }
}

impl NetworkRuntimeOwner {
    /// Finish read/collection transport using one original cleanup deadline.
    /// This is not provider/object absence or permission to drop unknown socket
    /// custody. The aggregate caller still owns those separate terminal proofs.
    ///
    /// # Safety
    /// The owned backend has ended and cannot submit another syscall callback.
    /// Keep this owner on any error; a timed-out driver still owns its thread,
    /// pending commands and all run custody independently of Tokio teardown.
    pub async unsafe fn finish_accepted_transport(
        &mut self,
        deadline: std::time::Instant,
    ) -> std::io::Result<()> {
        let deadline = {
            let mut original = self.shared.transport_terminal_deadline.lock().unwrap();
            *original.get_or_insert(deadline)
        };
        let controller = match self.shared.controller.lock().unwrap().as_ref() {
            None => {
                return if self.shared.accepted.lock().unwrap().collections_settled()? {
                    Ok(())
                } else {
                    Err(std::io::Error::other(
                        "accepted collection has no transport owner",
                    ))
                };
            }
            Some(Ok(controller)) => controller.clone(),
            Some(Err(error)) => return Err(std::io::Error::other(error.clone())),
        };
        let operation = async {
            self.shared
                .creations
                .lock()
                .await
                .finish_after_backend(&controller)
                .await?;
            let pending = self.shared.accepted.lock().unwrap().pending_collections();
            for (_, _, sequence) in pending {
                // Await only the already-submitted request. The native driver
                // survives timeout/cancellation and retains its eventual reply.
                let _ = controller.response(sequence).await;
                self.shared.retain_completed_collections(&controller)?;
            }
            // A completed transport response alone is not an applied receipt.
            // A dropped preparation waiter must not let an unknown reservation
            // pass terminalization merely because there was no later capture.
            for (owner, lease, request) in controller.accepted_preparations()? {
                if self
                    .shared
                    .accepted
                    .lock()
                    .unwrap()
                    .prepared_effect(owner, lease)?
                    .0
                    != request
                {
                    return Err(std::io::Error::other(
                        "accepted preparation was not applied to its exact custody",
                    ));
                }
            }
            if !self.shared.accepted.lock().unwrap().collections_settled()?
                || !controller.quiescent()?
            {
                return Err(std::io::Error::other(
                    "accepted terminal requests remain unresolved",
                ));
            }
            Ok(())
        };
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), operation)
            .await
            .map_err(|_| std::io::Error::other("accepted terminal transport deadline"))??;
        match self.shared.driver.lock().unwrap().as_mut() {
            Some(Ok(driver)) => driver.stop_and_join(deadline),
            Some(Err(error)) => Err(std::io::Error::other(error.clone())),
            None => Err(std::io::Error::other(
                "accepted native driver owner missing",
            )),
        }
    }
}

// Only the actual stopped ptrace boundary uses this synchronous operation.
// pidfd_getfd takes exec_update_lock; it is not generally nonblocking. The
// pinned task/MM admission and ptrace stop exclude an exec lock holder waiting
// on this same callback. This helper does not prove FD-slot generation alone.
fn capture_socket(pidfd: &OwnedFd, fd: i32) -> std::io::Result<OwnedFd> {
    let captured = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), fd, 0u32) };
    if captured < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(captured as i32) })
}

impl NetworkRuntimeResources {
    /// The actual backend completion path calls this before publishing trace or
    /// summary success. The outside recovery owner remains alive on any error.
    pub(crate) async fn finish_accepted_after_backend(&self) -> std::io::Result<()> {
        let mut owner = NetworkRuntimeOwner {
            shared: self.shared.clone(),
        };
        // This is a finite transport maintenance budget, not guest time. The
        // owner latches the first absolute deadline; retries cannot renew it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        unsafe { owner.finish_accepted_transport(deadline).await }
    }

    #[cfg(test)]
    pub(crate) fn accepted_collection_transport_fixture(
        &mut self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
    ) -> (NetworkRuntimeOwner, AcceptedCollectionPeer) {
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        let shared = std::sync::Arc::get_mut(&mut self.shared).unwrap();
        shared.endpoint = Some(unsafe { OwnedFd::from_raw_fd(fds[0]) });
        shared
            .accepted
            .lock()
            .unwrap()
            .retain_preparation(owner, lease, 7, 11)
            .unwrap();
        let peer = accepted_transport::AcceptedSession::new(
            unsafe { OwnedFd::from_raw_fd(fds[1]) },
            shared.incarnation,
        )
        .unwrap();
        (
            NetworkRuntimeOwner {
                shared: self.shared.clone(),
            },
            AcceptedCollectionPeer(peer),
        )
    }

    #[cfg(test)]
    pub(crate) fn accepted_collection_effect_status(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
    ) -> std::io::Result<Option<(i32, i32)>> {
        self.shared
            .accepted
            .lock()
            .unwrap()
            .collection_effect_status(owner, lease)
    }
    #[cfg(test)]
    pub(crate) fn accepted_custody_fixture(
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
        call: crate::network_replay::NetworkStreamCallId,
    ) -> Self {
        let mut accepts = accepted::AcceptedCustody::default();
        accepts.submit(owner, lease, call).unwrap();
        Self {
            shared: std::sync::Arc::new(RuntimeShared {
                guard: Mutex::default(),
                endpoint: None,
                controller: Mutex::default(),
                driver: Mutex::default(),
                transport_terminal_deadline: Mutex::default(),
                incarnation: [91; 16],
                physical: Mutex::default(),
                accepted: Mutex::new(accepts),
                listeners: Mutex::default(),
                creations: tokio::sync::Mutex::default(),
            }),
            drops: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn accepted_recovery_result(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
    ) -> std::io::Result<Option<Result<i32, i32>>> {
        self.shared
            .accepted
            .lock()
            .unwrap()
            .recovery_result(owner, lease)
    }
    /// Adopt the exact endpoint created in the container startup callback.
    ///
    /// # Safety
    /// The other endpoint must belong exclusively to this run's parent service
    /// established through the actual owned container startup. Both branches
    /// must close unrelated endpoint aliases before workload permission. The
    /// random incarnation must be bound to that same exchange, not loaded from
    /// guest input or serialized Config. The endpoint must be SOCK_SEQPACKET,
    /// nonblocking and CLOEXEC. Guest exec must preserve CLOEXEC: a raw numeric
    /// pre-exec close is not used because a prior Command hook can reuse that slot.
    pub unsafe fn from_authenticated_startup(
        endpoint: OwnedFd,
        incarnation: [u8; 16],
    ) -> (NetworkRuntimeOwner, Self) {
        let shared = std::sync::Arc::new(RuntimeShared {
            guard: Mutex::default(),
            endpoint: Some(endpoint),
            controller: Mutex::default(),
            driver: Mutex::default(),
            transport_terminal_deadline: Mutex::default(),
            incarnation,
            physical: Mutex::default(),
            accepted: Mutex::default(),
            listeners: Mutex::default(),
            creations: tokio::sync::Mutex::default(),
        });
        (
            NetworkRuntimeOwner {
                shared: shared.clone(),
            },
            Self {
                shared,
                #[cfg(test)]
                drops: None,
            },
        )
    }

    /// Adopt a guard-only runtime after the actual Container clone.
    ///
    /// # Safety
    /// This process is the owned PID-namespace controller, the supplied guard
    /// belongs to that exact startup exchange, and its separate owner remains
    /// outside every cancellable backend future. The deadline is the original
    /// Container startup deadline. No config/artifact can establish this proof.
    pub unsafe fn from_authenticated_guard(
        control: std::sync::Arc<dyn guard::NetworkGuardControl>,
        deadline: std::time::Instant,
    ) -> (NetworkRuntimeOwner, Self) {
        let shared = std::sync::Arc::new(RuntimeShared {
            guard: Mutex::new(Some(GuardRuntime {
                control,
                deadline,
                observer: None,
                initial: None,
            })),
            endpoint: None,
            controller: Mutex::default(),
            driver: Mutex::default(),
            transport_terminal_deadline: Mutex::default(),
            incarnation: [0; 16],
            physical: Mutex::default(),
            accepted: Mutex::default(),
            listeners: Mutex::default(),
            creations: tokio::sync::Mutex::default(),
        });
        (
            NetworkRuntimeOwner {
                shared: shared.clone(),
            },
            Self {
                shared,
                #[cfg(test)]
                drops: None,
            },
        )
    }

    /// Add the same-run guard to an accepted runtime after clone.
    ///
    /// # Safety
    /// The owned-controller and separate recovery-owner requirements of
    /// from_authenticated_guard apply to this exact existing runtime as well.
    pub unsafe fn attach_authenticated_guard(
        &self,
        control: std::sync::Arc<dyn guard::NetworkGuardControl>,
        deadline: std::time::Instant,
    ) -> std::io::Result<()> {
        let mut guard = self.shared.guard.lock().unwrap();
        if guard.is_some() {
            return Err(std::io::Error::other("guard already attached"));
        }
        *guard = Some(GuardRuntime {
            control,
            deadline,
            observer: None,
            initial: None,
        });
        Ok(())
    }

    /// Start one independently owned native observer before guest permission.
    /// A partial start is retained and never retried with a new abort token.
    pub fn start_guard_observer(&self) -> std::io::Result<Option<guard::NetworkGuardPublication>> {
        let mut guard = self.shared.guard.lock().unwrap();
        let Some(guard) = guard.as_mut() else {
            return Ok(None);
        };
        if let Some((publication, result)) = &guard.observer {
            return result
                .clone()
                .map(|()| Some(publication.clone()))
                .map_err(std::io::Error::other);
        }
        let abort = guard::NetworkGuardControllerAbort::owned_controller();
        let publication = abort.publication();
        guard.observer = Some((
            publication.clone(),
            Err("guard observer start submitted without result".into()),
        ));
        let result = guard
            .control
            .start_observer(abort)
            .map_err(|error| error.to_string());
        guard.observer.as_mut().unwrap().1 = result.clone();
        result
            .map(|()| Some(publication))
            .map_err(std::io::Error::other)
    }

    /// Called only after the real global birth/MM authentication identifies the
    /// initial task. Native registration runs without scheduler or task locks.
    pub(crate) fn register_guard_initial(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
    ) -> std::io::Result<()> {
        let mut retained = self.shared.guard.lock().unwrap();
        let Some(guard) = retained.as_mut() else {
            return Ok(());
        };
        if let Some((initial, result)) = &guard.initial {
            if initial.thread != owner.thread {
                return Err(std::io::Error::other("guard initial task changed"));
            }
            return result.clone().map_err(std::io::Error::other);
        }
        let task = self
            .shared
            .physical
            .lock()
            .unwrap()
            .get(owner)?
            .as_fd()
            .try_clone_to_owned()?;
        if !guard
            .observer
            .as_ref()
            .is_some_and(|(_, result)| result.is_ok())
        {
            return Err(std::io::Error::other(
                "guard initial task precedes actual observer startup",
            ));
        }
        guard.initial = Some((
            owner,
            Err("guard initial enrollment submitted without result".into()),
        ));
        let result = unsafe {
            guard
                .control
                .register_stopped_initial(task.as_fd(), guard.deadline)
        }
        .map_err(|error| error.to_string());
        guard.initial.as_mut().unwrap().1 = result.clone();
        result.map_err(std::io::Error::other)
    }

    fn accepted_controller(
        &self,
    ) -> std::io::Result<std::sync::Arc<accepted_controller::Controller>> {
        let mut retained = self.shared.controller.lock().unwrap();
        if retained.is_none() {
            let outcome = self
                .shared
                .endpoint
                .as_ref()
                .ok_or_else(|| std::io::Error::other("accepted runtime has no endpoint"))
                .and_then(|endpoint| endpoint.as_fd().try_clone_to_owned())
                .and_then(|endpoint| {
                    accepted_controller::Controller::new(endpoint, self.shared.incarnation)
                })
                .map(std::sync::Arc::new)
                .map_err(|error| error.to_string());
            if let Ok(controller) = &outcome {
                let shared = self.shared.clone();
                let worker_controller = controller.clone();
                *self.shared.driver.lock().unwrap() = Some(
                    accepted_driver::Driver::start(controller.clone(), move || {
                        shared.retain_completed_collections(&worker_controller)
                    })
                    .map_err(|error| error.to_string()),
                );
            }
            *retained = Some(outcome);
        }
        if let Some(Err(error)) = self.shared.driver.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(error.clone()));
        }
        retained
            .as_ref()
            .unwrap()
            .as_ref()
            .cloned()
            .map_err(|error| std::io::Error::other(error.clone()))
    }

    /// Called after authenticated model-call and task/MM admission before listen.
    /// Exact kernel slot-generation admission is still a separate activation
    /// requirement; a numeric descriptor plus this pin does not establish it.
    /// Actual ownership enters this run object before provider I/O can await.
    pub(crate) fn capture_accepted_listener(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        open_file: crate::types::OpenFileId,
        fd: i32,
    ) -> std::io::Result<()> {
        let tasks = self.shared.physical.lock().unwrap();
        // Current caller authority is distinct from the retained pin's origin.
        // This check also applies when another alias already owns custody.
        let task = tasks.get(owner)?;
        self.shared
            .listeners
            .lock()
            .unwrap()
            .capture(owner, call, open_file, fd, || capture_socket(task, fd))
    }

    pub(crate) async fn enroll_accepted_listener(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        open_file: crate::types::OpenFileId,
        state: &crate::network_replay::NetworkStreamSocketState,
    ) -> std::io::Result<accepted_listener::Enrollment> {
        let controller = self.accepted_controller()?;
        // Historical capture owner is never reused as current task authority.
        // An existing enrollment request recovers its exact retained response.
        self.shared.physical.lock().unwrap().get(owner)?;
        let sequence = controller.prepare(
            accepted_controller::Effect::Listener(open_file),
            owner,
            &accepted_provider::Request::Enroll {
                generation: state.option_generation,
            },
            || {
                let tasks = self.shared.physical.lock().unwrap();
                let listeners = self.shared.listeners.lock().unwrap();
                let (_, pin) = listeners.pin(open_file)?;
                Ok(vec![
                    pin.as_fd().try_clone_to_owned()?,
                    tasks.get(owner)?.as_fd().try_clone_to_owned()?,
                ])
            },
        )?;
        match controller.response(sequence).await? {
            accepted_provider::Reply::Command(observation) => {
                accepted_listener::Enrollment::checked(
                    open_file,
                    state,
                    u64::from_le_bytes(self.shared.incarnation[..8].try_into().unwrap()),
                    observation,
                )
            }
            _ => Err(std::io::Error::other(
                "listener enrollment returned another provider operation",
            )),
        }
    }

    /// Final observation retirement has its own exact ACK, so it does not
    /// require a future child. Aggregate shutdown must call this before provider
    /// teardown; it does not certify unresolved socket rights or controller exit.
    pub(crate) async fn finish_accepted_observations(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
    ) -> std::io::Result<()> {
        let controller = self.accepted_controller()?;
        self.shared
            .creations
            .lock()
            .await
            .finish(&controller, owner)
            .await
    }

    /// Read at most the next provider occurrence. None means observation is
    /// currently pending; it is not permission to report guest EAGAIN.
    pub(crate) async fn next_accepted_creation(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
    ) -> std::io::Result<Option<accepted_creation::Publication<'_>>> {
        let controller = self.accepted_controller()?;
        let mut cursor = self.shared.creations.lock().await;
        let provider = u64::from_le_bytes(self.shared.incarnation[..8].try_into().unwrap());
        let evidence = cursor.next(&controller, owner, provider).await?;
        Ok(evidence.map(|evidence| accepted_creation::Publication { evidence, cursor }))
    }

    pub(crate) async fn resolve_accepted_pin(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
    ) -> std::io::Result<accepted::Resolved> {
        let controller = self.accepted_controller()?;
        let sequence = controller.prepare(
            accepted_controller::Effect::Match(lease),
            owner,
            &accepted_provider::Request::ResolveAccepted,
            || {
                let tasks = self.shared.physical.lock().unwrap();
                let accepts = self.shared.accepted.lock().unwrap();
                Ok(vec![
                    accepts.pin(owner, lease)?.as_fd().try_clone_to_owned()?,
                    tasks.get(owner)?.as_fd().try_clone_to_owned()?,
                ])
            },
        )?;
        match controller.response(sequence).await? {
            accepted_provider::Reply::Command(observation) => {
                let matched = accepted::Resolved::checked(
                    u64::from_le_bytes(self.shared.incarnation[..8].try_into().unwrap()),
                    observation,
                )?;
                self.shared
                    .accepted
                    .lock()
                    .unwrap()
                    .confirm_resolved(owner, lease, matched)
            }
            _ => Err(std::io::Error::other(
                "accepted matching returned another provider operation",
            )),
        }
    }

    pub(crate) async fn prepare_accepted_effect(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
        listener: crate::types::OpenFileId,
        physical: crate::network_replay::accepted::AcceptedPhysicalIdentity,
        fd: i32,
        flags: i32,
    ) -> std::io::Result<()> {
        let controller = self.accepted_controller()?;
        let sequence = controller.prepare(
            accepted_controller::Effect::PrepareAccept(lease),
            owner,
            &accepted_provider::Request::PrepareAccept {
                identity: accepted_provider::Identity {
                    provider: physical.provider,
                    object: physical.object,
                    namespace: physical.namespace,
                },
                lease: lease.0,
                mm: owner.mm.generation(),
                fd,
                flags,
            },
            || {
                let tasks = self.shared.physical.lock().unwrap();
                let listeners = self.shared.listeners.lock().unwrap();
                Ok(vec![
                    listeners.pin(listener)?.1.as_fd().try_clone_to_owned()?,
                    tasks.get(owner)?.as_fd().try_clone_to_owned()?,
                ])
            },
        )?;
        match controller.response(sequence).await? {
            accepted_provider::Reply::Prepared(observation)
                if observation.status.returned == 0 && observation.raw != 0 =>
            {
                self.shared.accepted.lock().unwrap().retain_preparation(
                    owner,
                    lease,
                    sequence,
                    observation.raw,
                )
            }
            _ => Err(std::io::Error::other(
                "accepted effect preparation failed; original transport receipt retained",
            )),
        }
    }

    fn queue_accepted_collection(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
    ) -> std::io::Result<u64> {
        let mut custody = self.shared.accepted.lock().unwrap();
        if let Some(prior) = custody.collection_submission(owner, lease)? {
            return prior.map_err(std::io::Error::other);
        }
        // Retain every failure, including missing preparation/controller before
        // transport submission. A later caller cannot repair/retry unknown work
        // by changing identity or silently creating another request.
        let result = (|| {
            let (prepared_request, command) = custody.prepared_effect(owner, lease)?;
            let controller = self.accepted_controller()?;
            // The service borrows its original retained task pidfd. There is no
            // second rights transfer or command submission after owner exit.
            controller.prepare(
                accepted_controller::Effect::CollectAccept(lease),
                owner,
                &accepted_provider::Request::CollectAccept {
                    command,
                    prepared_request,
                },
                || Ok(vec![]),
            )
        })()
        .map_err(|error: std::io::Error| error.to_string());
        custody.retain_collection_submission(owner, lease, result.clone())?;
        result.map_err(std::io::Error::other)
    }

    pub(crate) async fn collect_accepted_effect(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
    ) -> std::io::Result<()> {
        let sequence = self.queue_accepted_collection(owner, lease)?;
        let controller = self.accepted_controller()?;
        let response = controller
            .response(sequence)
            .await
            .map_err(|error| error.to_string());
        let mut custody = self.shared.accepted.lock().unwrap();
        custody.complete_collection(owner, lease, sequence, response)?;
        custody.collection_result(owner, lease)
    }

    /// Registration is accepted only after the GlobalRPC callback independently
    /// checks its sender/process/MM. This private runtime exists solely in the
    /// actual ptrace/E9 TracerBuilder path, whose Guest::tid names that task.
    pub(crate) fn register_ptrace_task(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        process: i32,
        thread: i32,
    ) -> std::io::Result<()> {
        self.shared
            .physical
            .lock()
            .unwrap()
            .register(owner, process, thread, || {
                // PIDFD_THREAD (O_EXCL) binds this task's real files_struct, even
                // when CLONE_THREAD omitted CLONE_FILES. A process pidfd is wrong.
                let fd =
                    unsafe { libc::syscall(libc::SYS_pidfd_open, thread, libc::O_EXCL as u32) };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
            })
    }

    pub(crate) fn forget_task(&self, owner: crate::network_replay::NetworkStreamOwner) {
        self.shared.accepted.lock().unwrap().abandon(owner);
        self.shared.physical.lock().unwrap().forget(owner);
    }

    pub(crate) fn submit_accept(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
        call: crate::network_replay::NetworkStreamCallId,
    ) -> std::io::Result<()> {
        // A registered task is required before creating a physical operation.
        self.shared.physical.lock().unwrap().get(owner)?;
        self.shared
            .accepted
            .lock()
            .unwrap()
            .submit(owner, lease, call)
    }

    /// Synchronous first-poll completion used only by the admitted ptrace path.
    /// The raw result precedes acquisition; the actual OwnedFd is stored before
    /// even CLOEXEC validation, and before any provider request or reply.
    pub(crate) fn capture_accept_return(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
        result: Result<i32, i32>,
        acquisition_authorized: bool,
    ) -> std::io::Result<()> {
        let tasks = self.shared.physical.lock().unwrap();
        let mut accepts = self.shared.accepted.lock().unwrap();
        let captured = accepts.capture(
            owner,
            lease,
            result,
            |fd| {
                if !acquisition_authorized {
                    return Err(std::io::Error::other(
                        "late accept result retained without current physical task/MM authority",
                    ));
                }
                let pidfd = tasks.get(owner)?;
                capture_socket(pidfd, fd)
            },
            |pin| {
                let flags = unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFD) };
                if flags < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if flags & libc::FD_CLOEXEC == 0 {
                    return Err(std::io::Error::other(
                        "captured accept descriptor is not CLOEXEC",
                    ));
                }
                Ok(())
            },
        );
        // Capture errors (including a vanished current FD) do not erase the
        // historical kernel return. Queue its exact collection before replying
        // ThreadExited; the native driver owns further progress independently.
        let latched = accepts.captured_return_matches(owner, lease, result);
        drop(accepts);
        drop(tasks);
        if latched {
            // Capture acknowledgement attests only that the historical return
            // was latched. Collection submission has its own durable result;
            // Collect/terminal refuses its failure without changing that return.
            let _ = self.queue_accepted_collection(owner, lease);
        }
        captured
    }

    /// Private run identity, not a channel-match key or guest-visible input.
    pub fn incarnation(&self) -> [u8; 16] {
        self.shared.incarnation
    }

    /// Borrow the still-owned endpoint without granting numeric reattachment.
    pub fn endpoint(&self) -> BorrowedFd<'_> {
        self.shared
            .endpoint
            .as_ref()
            .expect("runtime endpoint has not been consumed")
            .as_fd()
    }
}

#[cfg(test)]
pub(crate) struct AcceptedCollectionPeer(accepted_transport::AcceptedSession);

#[cfg(test)]
impl AcceptedCollectionPeer {
    pub(crate) fn reply_to_collection(
        &mut self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
        deadline: std::time::Instant,
    ) {
        let sequence = loop {
            match self.0.try_receive().unwrap() {
                Some(accepted_transport::Received::Request(sequence)) => break sequence,
                None => {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                _ => panic!("unexpected collection peer message"),
            }
        };
        self.0
            .dispatch(sequence, |envelope, rights| {
                assert_eq!(envelope.owner, Some(owner));
                assert_eq!(envelope.accept, Some(lease));
                assert!(rights.is_empty());
                assert!(matches!(
                    serde_json::from_slice(&envelope.body),
                    Ok(accepted_provider::Request::CollectAccept {
                        command: 11,
                        prepared_request: 7
                    })
                ));
                serde_json::to_vec(&accepted_provider::Reply::AcceptedEffect(
                    accepted_provider::Observation {
                        status: accepted_provider::CallStatus {
                            operation: "controlled collection failure".into(),
                            returned: -1,
                            errno: Some(libc::EIO),
                        },
                        raw: accepted_provider_ffi::AcceptedEffect {
                            command: accepted_provider_ffi::CommandResult {
                                command: 11,
                                operation: 4,
                                returned: 8,
                                phase: 1,
                                ..Default::default()
                            },
                            installation: accepted_provider_ffi::FdAccept {
                                command: 11,
                                accept_lease: lease.0,
                                owner_mm: owner.mm.generation(),
                                ..Default::default()
                            },
                        }
                        .into(),
                    },
                ))
                .map_err(std::io::Error::other)
            })
            .unwrap();
        assert!(self.0.try_reply(sequence).unwrap());
    }
    pub(crate) fn no_other_request(&mut self) -> bool {
        self.0.try_receive().unwrap().is_none()
    }
}

#[cfg(test)]
impl NetworkRuntimeOwner {
    pub(crate) fn stop_collection_fixture(&mut self, deadline: std::time::Instant) {
        self.shared
            .driver
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .stop_and_join(deadline)
            .unwrap();
    }
}

#[cfg(test)]
impl Drop for NetworkRuntimeResources {
    fn drop(&mut self) {
        if let Some(drops) = &self.drops {
            drops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[derive(Debug)]
pub(crate) enum RuntimeHandoffError {
    Nested,
    Unused,
    Reused,
}

impl std::fmt::Display for RuntimeHandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Nested => "nested network runtime handoff",
            Self::Unused => {
                "network runtime resource was not consumed by GlobalTool initialization"
            }
            Self::Reused => {
                "network runtime resource was consumed by more than one GlobalTool initialization"
            }
        })
    }
}
impl std::error::Error for RuntimeHandoffError {}

struct RuntimeSlot {
    resource: Option<NetworkRuntimeResources>,
    consumed: bool,
}

tokio::task_local! { static NETWORK_RUNTIME: RefCell<RuntimeSlot>; }

/// Scope one owned resource around the actual TracerBuilder::spawn future.
/// Tokio task-local scope follows task migration and isolates concurrent runs.
/// A primary spawn error survives missing-consumption diagnostics unchanged.
/// No endpoint is stored in a process-global or thread-local fallback registry.
pub async fn with_network_runtime_resources<T>(
    resource: NetworkRuntimeResources,
    operation: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    if NETWORK_RUNTIME.try_with(|_| ()).is_ok() {
        return Err(RuntimeHandoffError::Nested.into());
    }
    NETWORK_RUNTIME
        .scope(
            RefCell::new(RuntimeSlot {
                resource: Some(resource),
                consumed: false,
            }),
            async {
                let outcome = operation.await;
                // Never replace an already-present backend error with cleanup metadata.
                let value = outcome?;
                if !NETWORK_RUNTIME.with(|slot| slot.borrow().consumed) {
                    return Err(RuntimeHandoffError::Unused.into());
                }
                Ok(value)
            },
        )
        .await
}

pub(crate) fn take_network_runtime_resources()
-> Result<Option<NetworkRuntimeResources>, RuntimeHandoffError> {
    NETWORK_RUNTIME
        .try_with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.consumed {
                return Err(RuntimeHandoffError::Reused);
            }
            slot.consumed = true;
            Ok(slot.resource.take())
        })
        .unwrap_or(Ok(None))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::*;
    fn fixture(incarnation: u8) -> (NetworkRuntimeResources, Arc<AtomicUsize>) {
        let drops = Arc::new(AtomicUsize::new(0));
        (
            NetworkRuntimeResources {
                shared: Arc::new(RuntimeShared {
                    guard: Mutex::default(),
                    endpoint: None,
                    controller: Mutex::default(),
                    driver: Mutex::default(),
                    transport_terminal_deadline: Mutex::default(),
                    incarnation: [incarnation; 16],
                    physical: Mutex::default(),
                    accepted: Mutex::default(),
                    listeners: Mutex::default(),
                    creations: tokio::sync::Mutex::default(),
                }),
                drops: Some(drops.clone()),
            },
            drops,
        )
    }

    #[tokio::test]
    async fn accepted_terminal_timeout_keeps_final_collection_driven_and_custodied() {
        let thread = crate::types::DetTid::from_raw(91);
        let who = crate::network_replay::NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let lease = crate::network_replay::NetworkAcceptLeaseId(17);
        let mut runtime = NetworkRuntimeResources::accepted_custody_fixture(
            who,
            lease,
            crate::network_replay::NetworkStreamCallId::controlled_fixture(5),
        );
        let (mut owner, mut peer) = runtime.accepted_collection_transport_fixture(who, lease);
        assert!(
            runtime
                .capture_accept_return(who, lease, Ok(8), false)
                .is_err()
        );
        let original = std::time::Instant::now() + std::time::Duration::from_millis(20);
        let error = unsafe { owner.finish_accepted_transport(original).await }.unwrap_err();
        assert!(error.to_string().contains("transport deadline"));
        assert_eq!(
            *owner.shared.transport_terminal_deadline.lock().unwrap(),
            Some(original)
        );
        // No caller is waiting after timeout. The same owned driver still
        // receives and applies the exact eventual response, with no resubmit.
        let recovery = std::time::Instant::now() + std::time::Duration::from_secs(2);
        peer.reply_to_collection(who, lease, recovery);
        while runtime
            .accepted_collection_effect_status(who, lease)
            .unwrap()
            != Some((-1, 8))
        {
            assert!(std::time::Instant::now() < recovery);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(peer.no_other_request());
        assert!(unsafe { owner.finish_accepted_transport(recovery).await }.is_err());
        assert_eq!(
            *owner.shared.transport_terminal_deadline.lock().unwrap(),
            Some(original)
        );
        owner.stop_collection_fixture(recovery);
    }

    #[tokio::test]
    async fn accepted_capture_ack_keeps_collection_submission_failure_terminal() {
        let thread = crate::types::DetTid::from_raw(91);
        let who = crate::network_replay::NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let lease = crate::network_replay::NetworkAcceptLeaseId(17);
        let runtime = NetworkRuntimeResources::accepted_custody_fixture(
            who,
            lease,
            crate::network_replay::NetworkStreamCallId::controlled_fixture(5),
        );
        // Historical EAGAIN capture needs no FD. Its ACK does not assert that
        // the deliberately absent provider preparation/transport succeeded.
        runtime
            .capture_accept_return(who, lease, Err(libc::EAGAIN), false)
            .unwrap();
        let original = runtime
            .queue_accepted_collection(who, lease)
            .unwrap_err()
            .to_string();
        assert_eq!(original, "accept provider command not prepared");
        let stale = crate::network_replay::NetworkStreamOwner {
            mm: who.mm.for_exec(thread),
            ..who
        };
        assert!(runtime.queue_accepted_collection(stale, lease).is_err());
        assert_eq!(
            runtime
                .queue_accepted_collection(who, lease)
                .unwrap_err()
                .to_string(),
            original
        );
        assert!(runtime.collect_accepted_effect(who, lease).await.is_err());
        let mut owner = NetworkRuntimeOwner {
            shared: runtime.shared.clone(),
        };
        assert!(
            unsafe {
                owner
                    .finish_accepted_transport(
                        std::time::Instant::now() + std::time::Duration::from_secs(1),
                    )
                    .await
            }
            .is_err()
        );
        assert!(runtime.shared.controller.lock().unwrap().is_none());
        assert!(runtime.shared.driver.lock().unwrap().is_none());
        assert_eq!(
            runtime.accepted_recovery_result(who, lease).unwrap(),
            Some(Err(libc::EAGAIN))
        );
    }

    #[tokio::test]
    async fn accepted_terminal_refuses_prepared_effect_with_zero_applied_receipts() {
        let thread = crate::types::DetTid::from_raw(91);
        let who = crate::network_replay::NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let lease = crate::network_replay::NetworkAcceptLeaseId(17);
        let mut runtime = NetworkRuntimeResources::accepted_custody_fixture(
            who,
            lease,
            crate::network_replay::NetworkStreamCallId::controlled_fixture(5),
        );
        let (mut owner, mut peer) = runtime.accepted_collection_transport_fixture(who, lease);
        let error = unsafe {
            owner
                .finish_accepted_transport(
                    std::time::Instant::now() + std::time::Duration::from_secs(1),
                )
                .await
        }
        .unwrap_err();
        assert!(error.to_string().contains("no collection receipt"));
        assert!(peer.no_other_request());
        assert!(owner.shared.driver.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn concurrent_scopes_consume_only_their_own_resource_once() {
        let (first, a) = fixture(1);
        let (second, b) = fixture(2);
        let run = |resource, expected| {
            with_network_runtime_resources(resource, async move {
                tokio::task::yield_now().await;
                let owned = take_network_runtime_resources()?.unwrap();
                assert_eq!(owned.incarnation(), [expected; 16]);
                assert!(matches!(
                    take_network_runtime_resources(),
                    Err(RuntimeHandoffError::Reused)
                ));
                tokio::task::yield_now().await;
                drop(owned);
                Ok(())
            })
        };
        let (x, y) = tokio::join!(run(first, 1), run(second, 2));
        x.unwrap();
        y.unwrap();
        assert_eq!(a.load(Ordering::SeqCst), 1);
        assert_eq!(b.load(Ordering::SeqCst), 1);
        assert!(take_network_runtime_resources().unwrap().is_none());
    }

    #[tokio::test]
    async fn nested_scope_refuses_without_polling_or_stealing_outer() {
        let (outer, a) = fixture(1);
        let (inner, b) = fixture(2);
        with_network_runtime_resources(outer, async {
            let polled = AtomicUsize::new(0);
            let nested = with_network_runtime_resources(inner, async {
                polled.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap_err();
            assert_eq!(polled.load(Ordering::SeqCst), 0);
            assert!(nested.downcast_ref::<RuntimeHandoffError>().is_some());
            assert_eq!(
                take_network_runtime_resources()?.unwrap().incarnation(),
                [1; 16]
            );
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(a.load(Ordering::SeqCst), 1);
        assert_eq!(b.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unused_or_cancelled_scope_drops_only_its_unconsumed_resource() {
        let (unused, a) = fixture(1);
        assert!(
            with_network_runtime_resources(unused, async { Ok(()) })
                .await
                .unwrap_err()
                .downcast_ref::<RuntimeHandoffError>()
                .is_some()
        );
        assert_eq!(a.load(Ordering::SeqCst), 1);
        let (cancelled, b) = fixture(2);
        let mut future = Box::pin(with_network_runtime_resources(
            cancelled,
            std::future::pending::<anyhow::Result<()>>(),
        ));
        assert!(futures::poll!(future.as_mut()).is_pending());
        drop(future);
        assert_eq!(b.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn primary_spawn_error_is_not_replaced_by_unused_resource_error() {
        let (resource, drops) = fixture(1);
        let error = with_network_runtime_resources(resource, async {
            Err::<(), _>(anyhow::anyhow!("primary spawn failure"))
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "primary spawn failure");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn scoped_handoff_follows_future_between_os_threads() {
        let (resource, drops) = fixture(7);
        let mut first_poll = true;
        let future = Box::pin(with_network_runtime_resources(resource, async move {
            let first = std::thread::current().id();
            std::future::poll_fn(move |cx| {
                if first_poll {
                    first_poll = false;
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                } else {
                    std::task::Poll::Ready(())
                }
            })
            .await;
            assert_ne!(first, std::thread::current().id());
            assert_eq!(
                take_network_runtime_resources()?.unwrap().incarnation(),
                [7; 16]
            );
            Ok(())
        }));
        let future = std::thread::spawn(move || {
            let mut future = future;
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(future.as_mut().poll(&mut cx).is_pending());
            future
        })
        .join()
        .unwrap();
        std::thread::spawn(move || futures::executor::block_on(future).unwrap())
            .join()
            .unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_initializer_drops_its_consumed_object_once() {
        let (resource, drops) = fixture(8);
        let mut future = Box::pin(with_network_runtime_resources(resource, async {
            let _resource = take_network_runtime_resources()?.unwrap();
            std::future::pending::<()>().await;
            Ok(())
        }));
        assert!(futures::poll!(future.as_mut()).is_pending());
        drop(future);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(take_network_runtime_resources().unwrap().is_none());
    }

    #[tokio::test]
    async fn recovery_owner_outlives_cancelled_backend_future() {
        let (resource, drops) = fixture(9);
        let owner = NetworkRuntimeOwner {
            shared: resource.shared.clone(),
        };
        let observed = Arc::downgrade(&owner.shared);
        let mut future = Box::pin(with_network_runtime_resources(resource, async {
            let _inside = take_network_runtime_resources()?.unwrap();
            std::future::pending::<()>().await;
            Ok(())
        }));
        assert!(futures::poll!(future.as_mut()).is_pending());
        drop(future);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(observed.upgrade().is_some());
        assert_eq!(owner.shared.incarnation, [9; 16]);
        drop(owner);
        assert!(observed.upgrade().is_none());
    }
}

#[cfg(test)]
pub(crate) fn custody_identity_fixture(
    owner: crate::network_replay::NetworkStreamOwner,
) -> impl std::fmt::Debug {
    let mut identities = physical::CustodyTasks::<()>::default();
    identities
        .register(owner, owner.thread.as_raw(), owner.thread.as_raw(), || {
            Ok(())
        })
        .unwrap();
    identities
}
