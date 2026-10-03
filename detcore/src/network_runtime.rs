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
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;
use std::sync::Mutex;

pub(crate) mod accepted;
mod accepted_controller;
pub(crate) use accepted_controller::copy_wire_authority;
mod provider_topology;
pub use provider_topology::ProviderTopology;
mod provider_wire;
pub use provider_wire::ProviderWireFormat;
pub(crate) mod accepted_creation;
mod accepted_driver;
pub(crate) mod accepted_listener;
mod accepted_parent;
mod accepted_provider;
pub(crate) use accepted_provider::OriginalResult;
pub(crate) use accepted_provider::OriginalSelection;
pub(crate) use accepted_provider::PidfdIdentity;
mod accepted_provider_ffi;
mod accepted_service;
mod accepted_transport;
pub mod capability_unit;
mod fd_journal;
mod grouped_broker;
pub mod guard;
pub(crate) mod guard_probe;
mod helper_receive;
pub(crate) mod native_peer;
pub(crate) use helper_receive::Binding as HelperCopyBinding;
pub(crate) use helper_receive::Completion as HelperCopyCompletion;
mod foreground_epoll;
pub(crate) mod installation_observation;
mod joined_native_worker;
pub(crate) mod native_birth;
pub(crate) mod native_birth_outcome;
mod native_copy_exclusion;
mod native_source_interval;
mod openat_observation;
pub(crate) mod original_connect;
pub(crate) mod original_epoll_ctl;
pub(crate) mod original_installation;
pub(crate) mod original_read_copy;
pub(crate) mod original_send;
mod physical;
pub(crate) mod shared_waits;
pub(crate) mod socket_profile;
mod terminal_socket_observation;
// Both host C bridges use one supervisor module, including its unchanged
// process-group tests, rather than registering that source twice.
#[cfg(test)]
#[path = "../../hermit-cli/network-provider/driver-ftrace-inputs.rs"]
mod driver_ftrace_inputs;
#[cfg(test)]
#[path = "../../hermit-cli/network-provider/driver-ftrace-process.rs"]
mod driver_ftrace_process;
#[cfg(test)]
#[path = "../../hermit-cli/network-provider/process_group.rs"]
mod process_group;
pub use accepted_parent::AcceptedPostSpawn;
pub use accepted_parent::AcceptedProviderLaunch;
pub use accepted_parent::AcceptedSpawned;
pub use accepted_parent::GroupedBootstrapTransport;
pub use accepted_parent::ParentAcceptedService;
pub use accepted_parent::ParentAcceptedStartFailure;
pub use accepted_parent::ProviderArtifact;
pub use accepted_service::run_accepted_provider_process;
pub(crate) use foreground_epoll::ForegroundEntryAdmission;
pub(crate) use foreground_epoll::JoinedNativePrefix;
#[cfg(test)]
pub(crate) use foreground_epoll::controlled_joined_prefix;
pub use grouped_broker::GroupedParentOwner;
pub use grouped_broker::run_grouped_leaf_delegate_process;
pub use grouped_broker::run_grouped_runtime_keeper_process;
pub use grouped_broker::run_grouped_source_owner_process;
pub use grouped_broker::run_grouped_source_process;
pub use grouped_broker::run_grouped_startup_controller_process;
pub(crate) use joined_native_worker::JoinedNativeWorkerReceipt;
pub(crate) use native_copy_exclusion::NativeCopyExclusion;
pub(crate) use native_copy_exclusion::ReceiveRetryAdmission;
pub(crate) use native_copy_exclusion::ReceiveRetryOrigin;
pub(crate) use native_source_interval::NativeSourceInterval;
pub(crate) use physical::ForegroundRoot;
pub(crate) use physical::SharedForegroundLineage;
#[cfg(test)]
pub(crate) use physical::InitialDescriptor;
pub(crate) use physical::InitialFileStat;
pub(crate) use physical::InitialMetadataIdentity;
pub(crate) use physical::InitialTableAssociation;
pub use physical::InitialTableClaim;
pub use physical::InitialTableTicket;
pub use physical::InitialTableView;
#[cfg(test)]
pub(crate) use physical::changed_initial_root_fixture;
pub(crate) use physical::check_initial_stats;
#[cfg(test)]
pub(crate) use physical::controlled_foreground_root;
#[cfg(test)]
pub(crate) use physical::controlled_foreground_runtime;
#[cfg(test)]
pub(crate) use physical::initial_root_fixture;
pub(crate) use physical::required_initial_metadata;

/// One run's controller endpoint, delivered by the authenticated startup path.
/// It is deliberately neither Clone nor Serialize; a copied integer cannot
/// reconstruct this ownership capability.
#[derive(Debug)]
pub struct NetworkRuntimeResources {
    shared: std::sync::Arc<RuntimeShared>,
    #[cfg(test)]
    drops: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    #[cfg(test)]
    controlled_private_drain: Mutex<Option<native_peer::ControlledPrivateDrain>>,
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
    copy_wire: Option<ProviderWireFormat>,
    physical: Mutex<physical::CustodyTasks<OwnedFd>>,
    accepted: Mutex<accepted::AcceptedCustody<OwnedFd>>,
    listeners: Mutex<accepted_listener::Listeners<OwnedFd>>,
    creations: tokio::sync::Mutex<accepted_creation::Creations>,
    fd_journal: tokio::sync::Mutex<fd_journal::Journal>,
    native_streams: Mutex<native_peer::Calls>,
    native_workers: Mutex<NativeWorkers>,
    native_terminal_failure: Mutex<Option<String>>,
}

// Execution handles, not semantic call/lease authority. Each native operation
// retains its handle here until an actual JoinHandle result is observed. A
// canceled callback cannot detach a worker from run-level terminal ownership.
type NativeWorkerHandle = std::sync::Arc<NativeWorker>;

#[derive(Debug, Default)]
struct NativeWorkers {
    closed: bool,
    copy_exclusion: Option<std::sync::Arc<native_copy_exclusion::Interval>>,
    // Weak locally: the caller and backend's actual join registry retain the
    // interval across cancellation, without introducing an ownership cycle.
    source_read: std::sync::Weak<native_source_interval::Interval>,
    tasks: Vec<NativeWorkerHandle>,
    // A joined prefix is invalidated by every actual new submission. This is
    // execution bookkeeping, never semantic Call or physical result authority.
    submission_generation: u64,
}

impl NativeWorkers {
    fn source_read_active(&self) -> bool {
        self.source_read.upgrade().is_some()
    }
}

#[derive(Debug)]
struct NativeWorker {
    // The actual spawning owner, never reconstructed from an execution ID.
    runtime: std::sync::Weak<RuntimeShared>,
    completion: tokio::sync::Mutex<NativeWorkerCompletion>,
    // Never protected by the lock held across JoinHandle::await. Cleanup must
    // be able to latch its deadline while a live callback owns that join lock.
    deadline_failure: Mutex<Option<String>>,
}

/// One existing worker's retained custody, not a keyed task/call registry.
/// The wrapper owns this cell after FnOnce captures have been destroyed, so
/// deleting a Call or canceling its receiver cannot release uncertain buffers,
/// task capabilities or raw evidence back to a reusable pooled worker.
#[derive(Default)]
struct NativeQuarantine {
    possible: std::sync::atomic::AtomicBool,
    custody: Mutex<Option<Box<dyn Send>>>,
}
impl NativeQuarantine {
    fn retain(&self, custody: impl Send + 'static) -> std::io::Result<()> {
        let mut owned = self.custody.lock().unwrap();
        if owned.is_some() || self.is_possible() {
            return Err(std::io::Error::other(
                "native quarantine changed original custody",
            ));
        }
        *owned = Some(Box::new(custody));
        Ok(())
    }
    fn mark_possible(&self) -> std::io::Result<()> {
        if self.custody.lock().unwrap().is_none() || self.is_possible() {
            return Err(std::io::Error::other(
                "native auxiliary submission lacks exact quarantine custody",
            ));
        }
        self.possible
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    fn is_possible(&self) -> bool {
        self.possible.load(std::sync::atomic::Ordering::Acquire)
    }
    // Call only after the exact provider idle retirement and transport group
    // retirement. This flag never certifies semantic success or object absence.
    fn retired(&self) {
        self.possible
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

#[derive(Debug)]
struct NativeWorkerCompletion {
    task: Option<tokio::task::JoinHandle<Result<(), String>>>,
    terminal: Option<Result<(), String>>,
}

/// Completion authority retained by the run-owned capture worker and the
/// owner-exit continuation. No guest callback may use it to publish a result.
#[derive(Clone)]
pub(crate) struct NativeCaptureRecovery {
    engine: std::sync::Arc<Mutex<crate::network_replay::NetworkReplayEngine>>,
    changed: std::sync::Arc<tokio::sync::Notify>,
    retire_ports: std::sync::Arc<dyn Fn(Vec<detcore_model::fd::OpenFileId>) + Send + Sync>,
    terminal_allocator: Option<std::sync::Arc<original_installation::TerminalPublisher>>,
    record_network: bool,
}

impl std::fmt::Debug for NativeCaptureRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeCaptureRecovery")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
enum NativeRetirement {
    Original(
        crate::network_replay::NetworkStreamOwner,
        crate::network_replay::NetworkStreamCallId,
    ),
    Stream(crate::network_replay::native_terminal::Admission),
}

impl NativeCaptureRecovery {
    /// Borrow the existing engine without cloning or transferring custody.
    pub(crate) fn engine(&self) -> &Mutex<crate::network_replay::NetworkReplayEngine> {
        self.engine.as_ref()
    }

    pub(crate) fn new(
        engine: std::sync::Arc<Mutex<crate::network_replay::NetworkReplayEngine>>,
        changed: std::sync::Arc<tokio::sync::Notify>,
        retire_ports: impl Fn(Vec<detcore_model::fd::OpenFileId>) + Send + Sync + 'static,
    ) -> Self {
        Self {
            engine,
            changed,
            retire_ports: std::sync::Arc::new(retire_ports),
            terminal_allocator: None,
            record_network: false,
        }
    }

    pub(crate) fn with_terminal_allocator(
        mut self,
        record_network: bool,
        publish: impl Fn(
            crate::network_replay::NetworkStreamOwner,
            &crate::network_replay::original_connect::Admission,
            &original_installation::Installation,
            original_installation::TerminalProfile,
        ) -> Result<
            crate::types::FdSlotBinding,
            crate::network_replay::NetworkReplayError,
        > + Send
        + Sync
        + 'static,
    ) -> Self {
        self.record_network = record_network;
        self.terminal_allocator = Some(std::sync::Arc::new(publish));
        self
    }

    fn retire_known(
        &self,
        shared: &RuntimeShared,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
    ) -> std::io::Result<bool> {
        let Some(outcome) = shared
            .native_streams
            .lock()
            .unwrap()
            .capture_outcome(owner, call)?
        else {
            // The original worker is still responsible for the unknown result.
            return Ok(false);
        };
        if !self
            .engine
            .lock()
            .unwrap()
            .begin_abandoned_native_capture(owner, call)
            .map_err(std::io::Error::other)?
        {
            return Ok(false);
        }
        if outcome.is_ok() {
            let work = shared
                .native_streams
                .lock()
                .unwrap()
                .prepare_release(owner, call)?;
            let release = work.perform(); // May linger; no shared mutex is held.
            shared
                .native_streams
                .lock()
                .unwrap()
                .retain_release(owner, call, release)?;
        }
        let retired = {
            let mut engine = self.engine.lock().unwrap();
            engine
                .finish_abandoned_native_capture(owner, call)
                .map_err(std::io::Error::other)?;
            engine.take_lifetime_retired_ports().into_iter().collect()
        };
        match outcome {
            Ok(()) => shared
                .native_streams
                .lock()
                .unwrap()
                .finish_release(owner, call)?,
            Err(errno) => shared
                .native_streams
                .lock()
                .unwrap()
                .finish_failed_capture(owner, call, errno)?,
        }
        (self.retire_ports)(retired);
        self.changed.notify_waiters();
        Ok(true)
    }
}

impl RuntimeShared {
    fn start_native_worker<T: Send + 'static>(
        self: &std::sync::Arc<Self>,
        executor: tokio::runtime::Handle,
        operation: impl FnOnce() -> std::io::Result<T> + Send + 'static,
    ) -> std::io::Result<(
        NativeWorkerHandle,
        tokio::sync::oneshot::Receiver<std::io::Result<T>>,
    )> {
        self.start_native_worker_inner(executor, None, false, operation)
    }
    fn start_native_release_worker<T: Send + 'static>(
        self: &std::sync::Arc<Self>,
        executor: tokio::runtime::Handle,
        operation: impl FnOnce() -> std::io::Result<T> + Send + 'static,
    ) -> std::io::Result<(
        NativeWorkerHandle,
        tokio::sync::oneshot::Receiver<std::io::Result<T>>,
    )> {
        self.start_native_worker_inner(executor, None, true, operation)
    }
    fn start_original_retirement_worker<T: Send + 'static>(
        self: &std::sync::Arc<Self>,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        executor: tokio::runtime::Handle,
        operation: impl FnOnce() -> std::io::Result<T> + Send + 'static,
    ) -> std::io::Result<(
        NativeWorkerHandle,
        tokio::sync::oneshot::Receiver<std::io::Result<T>>,
    )> {
        self.start_native_worker_inner(
            executor,
            Some(NativeRetirement::Original(owner, call)),
            false,
            operation,
        )
    }
    fn start_native_worker_inner<T: Send + 'static>(
        self: &std::sync::Arc<Self>,
        executor: tokio::runtime::Handle,
        retiring: Option<NativeRetirement>,
        require_idle: bool,
        operation: impl FnOnce() -> std::io::Result<T> + Send + 'static,
    ) -> std::io::Result<(
        NativeWorkerHandle,
        tokio::sync::oneshot::Receiver<std::io::Result<T>>,
    )> {
        self.start_native_worker_with_quarantine(executor, retiring, require_idle, None, operation)
    }
    fn start_native_worker_with_quarantine<T: Send + 'static>(
        self: &std::sync::Arc<Self>,
        executor: tokio::runtime::Handle,
        retiring: Option<NativeRetirement>,
        require_idle: bool,
        quarantine: Option<std::sync::Arc<NativeQuarantine>>,
        operation: impl FnOnce() -> std::io::Result<T> + Send + 'static,
    ) -> std::io::Result<(
        NativeWorkerHandle,
        tokio::sync::oneshot::Receiver<std::io::Result<T>>,
    )> {
        let (send, receive) = tokio::sync::oneshot::channel();
        let worker = {
            let mut owned = self.native_workers.lock().unwrap();
            if owned.source_read_active() {
                let error = "native submission attempted during retained source-read interval".to_string();
                self.native_terminal_failure.lock().unwrap().get_or_insert(error.clone());
                return Err(std::io::Error::other(error));
            }
            if require_idle && !owned.tasks.is_empty() {
                return Err(std::io::Error::other(
                    "native stream release requires all prior workers to be joined",
                ));
            }
            if owned
                .copy_exclusion
                .as_ref()
                .is_some_and(|copy| copy.active())
            {
                let error =
                    "native submission attempted during retained guest-copy exclusion".to_string();
                self.native_terminal_failure
                    .lock()
                    .unwrap()
                    .get_or_insert(error.clone());
                return Err(std::io::Error::other(error));
            }
            // This exception cannot admit a capture or new effect. It names
            // an existing Calls entry whose exact provider/transport custody
            // has already retired and whose one physical close is latched.
            if let Some(retiring) = retiring {
                let mut calls = self.native_streams.lock().unwrap();
                match retiring {
                    NativeRetirement::Original(owner, call) => {
                        let original = calls.original(owner, call)?;
                        if !original.retired || !original.close_queued {
                            return Err(std::io::Error::other(
                                "late retirement lacks an owned retired original call",
                            ));
                        }
                    }
                    NativeRetirement::Stream(admission) => {
                        if !owned.closed {
                            return Err(std::io::Error::other(
                                "terminal stream retirement before cutoff",
                            ));
                        }
                        calls.terminal_release_authorized(admission)?;
                    }
                }
            }
            if owned.closed && retiring.is_none() {
                let error =
                    "native physical submission after terminal admission closed".to_string();
                self.native_terminal_failure
                    .lock()
                    .unwrap()
                    .get_or_insert(error.clone());
                return Err(std::io::Error::other(error));
            }
            let next_generation = owned.submission_generation.checked_add(1).ok_or_else(|| {
                std::io::Error::other("native worker submission generation exhausted")
            })?;
            // A quarantined thread itself retains the run owner, including
            // same-Pending scratch/PIDFD evidence, even if callbacks and outside
            // Rust owners are dropped during failed controller shutdown.
            let quarantine_custody = quarantine.as_ref().map(|_| self.clone());
            let task = executor.spawn_blocking(move || {
                let retained_quarantine_custody = quarantine_custody;
                // A panic after possible registration must also deliver RED
                // and quarantine this exact thread, not unwind into the pool.
                let result = if quarantine.is_some() {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
                        .unwrap_or_else(|panic| {
                            let detail = panic
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                                .unwrap_or_else(|| "non-string panic".into());
                            Err(std::io::Error::other(format!(
                                "native helper worker panicked: {detail}"
                            )))
                        })
                } else {
                    operation()
                };
                let quarantined = quarantine.as_ref().is_some_and(|q| q.is_possible());
                let result = if quarantined && result.is_ok() {
                    Err(std::io::Error::other(
                        "quarantined helper cannot return idle success",
                    ))
                } else {
                    result
                };
                if let (Some(custody), Err(error)) = (&retained_quarantine_custody, &result) {
                    // Diagnostic poisoning must not unwind this wrapper back
                    // into Tokio's pool while its task command is uncertain.
                    custody
                        .native_terminal_failure
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_or_insert_with(|| error.to_string());
                }
                let terminal = result.as_ref().map(|_| ()).map_err(ToString::to_string);
                // Each operation has already retained its physical result in the
                // run-owned custody before it sends this dispensable callback reply.
                let _ = send.send(result);
                // Deliver the retained failure first. Returning this thread to
                // Tokio while a provider command might still name it would
                // let unrelated pooled work execute under that command. Only
                // actual controller process termination releases quarantine;
                // closed admission, canceled callbacks and timeouts do not.
                if quarantined {
                    loop {
                        std::thread::park();
                    }
                }
                drop(retained_quarantine_custody);
                terminal
            });
            let worker = std::sync::Arc::new(NativeWorker {
                runtime: std::sync::Arc::downgrade(self),
                completion: tokio::sync::Mutex::new(NativeWorkerCompletion {
                    task: Some(task),
                    terminal: None,
                }),
                deadline_failure: Mutex::default(),
            });
            owned.submission_generation = next_generation;
            owned.tasks.push(worker.clone());
            worker
        };
        Ok((worker, receive))
    }
}

impl RuntimeShared {
    async fn join_native_worker(&self, worker: &NativeWorkerHandle) -> std::io::Result<()> {
        let mut state = worker.completion.lock().await;
        if state.terminal.is_none() {
            // Await by mutable reference. Cancellation drops the lock guard,
            // leaving the actual JoinHandle retained in the run-owned object.
            let result = match state.task.as_mut().expect("unjoined native task").await {
                Ok(result) => result,
                Err(error) => Err(format!("native blocking worker failed: {error}")),
            };
            state.task.take();
            state.terminal = Some(result);
        }
        let result = match &*worker.deadline_failure.lock().unwrap() {
            Some(error) => Err(error.clone()),
            None => state.terminal.as_ref().expect("joined native task").clone(),
        };
        drop(state);
        if result.is_ok() {
            self.native_workers
                .lock()
                .unwrap()
                .tasks
                .retain(|owned| !std::sync::Arc::ptr_eq(owned, worker));
        }
        result.map_err(std::io::Error::other)
    }

    async fn finish_native_workers(&self, deadline: std::time::Instant) -> std::io::Result<()> {
        // Closing admission and taking the snapshot share the same mutex as
        // spawn plus handle insertion. Canceled backend futures need not prove
        // every independently spawned callback has already ended. A callback
        // arriving after this cutoff cannot start another physical operation.
        // Keep the original deadline, never a renewed per-worker allowance.
        let workers = {
            let mut owned = self.native_workers.lock().unwrap();
            owned.closed = true;
            owned.tasks.clone()
        };
        let mut failure = None;
        for worker in workers {
            match tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                self.join_native_worker(&worker),
            )
            .await
            {
                Ok(Err(error)) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
                Ok(Ok(())) => {}
                Err(_) => {
                    let error = "native blocking worker exceeded original terminal deadline; custody retained".to_string();
                    worker
                        .deadline_failure
                        .lock()
                        .unwrap()
                        .get_or_insert(error.clone());
                    self.native_terminal_failure
                        .lock()
                        .unwrap()
                        .get_or_insert(error.clone());
                    if failure.is_none() {
                        failure = Some(std::io::Error::other(error));
                    }
                }
            }
        }
        if !self.native_workers.lock().unwrap().tasks.is_empty() && failure.is_none() {
                failure = Some(std::io::Error::other("native workers remain unjoined"));
        }
        if let Some(error) = self.native_terminal_failure.lock().unwrap().as_ref()
            && failure.is_none()
        {
                failure = Some(std::io::Error::other(error.clone()));
            }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl RuntimeShared {
    // No fresh effect can enter after cutoff. Failed worker results still prove
    // termination; timeout, a live join or a lost/unjoined task does not.
    fn native_workers_terminated(&self) -> bool {
        let owned = self.native_workers.lock().unwrap();
        owned.closed
            && !owned.source_read_active()
            && !owned
                .copy_exclusion
                .as_ref()
                .is_some_and(|copy| copy.active())
            && owned.tasks.iter().all(|worker| {
                worker
                    .completion
                    .try_lock()
                    .is_ok_and(|state| state.terminal.is_some())
            })
    }

    async fn finish_native_after_backend(
        self: &std::sync::Arc<Self>,
        deadline: std::time::Instant,
    ) -> std::io::Result<()> {
        // This path is reached only after the actual owned backend has ended.
        // A canceled local copy may have stored bytes; retain a failed interval
        // before authorizing physical terminal retirement.
        self.abandon_copy_exclusion_after_backend();
        let primary = self.finish_native_workers(deadline).await;
        let retirement = self.finish_terminal_streams(deadline).await;
        primary.and(retirement)
    }

    async fn finish_terminal_streams(
        self: &std::sync::Arc<Self>,
        deadline: std::time::Instant,
    ) -> std::io::Result<()> {
        if !self.native_workers_terminated() {
            return Err(std::io::Error::other(
                "ordinary pin retirement still has live native workers",
            ));
        }
        let retained = self.native_streams.lock().unwrap().ordinary_retirements();
        let mut failure = None;
        for (owner, call, publication) in retained {
            let retired = async {
                let admission = publication
                    .engine
                    .lock()
                    .unwrap()
                    .terminal_stream_admission(owner, call)
                    .map_err(std::io::Error::other)?;
                let Some(admission) = admission else {
                    return Ok(());
                };
                // Preserve primary semantic failure even when physical cleanup
                // succeeds. This is never a late successful receive/copy/trace.
                self.native_terminal_failure
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| {
                        "ordinary stream task ended with incomplete semantic operation".into()
                    });
                let already_closed = self
                    .native_streams
                    .lock()
                    .unwrap()
                    .known_release(owner, call)?;
                if !already_closed && std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::other(
                        "ordinary pin retirement exceeded original terminal deadline",
                    ));
                }
                let launch = self
                    .native_streams
                    .lock()
                    .unwrap()
                    .claim_terminal_release(admission)?;
                if launch {
                    let shared = self.clone();
                    let _ = self.start_native_worker_inner(
                        tokio::runtime::Handle::current(),
                        Some(NativeRetirement::Stream(admission)),
                        false,
                        move || {
                            let work = shared
                                .native_streams
                                .lock()
                                .unwrap()
                                .prepare_terminal_release(admission)?;
                            let observed = work.perform();
                            shared
                                .native_streams
                                .lock()
                                .unwrap()
                                .retain_release(owner, call, observed)
                        },
                    )?;
                }
                // The close worker and raw close result survive cancellation
                // of this future in the same registry as every earlier effect.
                let joined = self.finish_native_workers(deadline).await;
                if !self.native_workers_terminated() {
                    return joined;
                }
                let evidence = self
                    .native_streams
                    .lock()
                    .unwrap()
                    .terminal_evidence(admission)?;
                let ports = {
                    let mut engine = publication.engine.lock().unwrap();
                    engine
                        .retain_terminal_stream_release(admission, evidence)
                        .map_err(std::io::Error::other)?;
                    engine.take_lifetime_retired_ports().into_iter().collect()
                };
                self.native_streams
                    .lock()
                    .unwrap()
                    .finish_release(owner, call)?;
                (publication.retire_ports)(ports);
                publication.changed.notify_waiters();
                joined
            }
            .await;
            if let Err(error) = retired
                && failure.is_none() {
                    failure = Some(error);
                }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[derive(Debug)]
struct GuardRuntime {
    control: std::sync::Arc<dyn guard::NetworkGuardControl>,
    deadline: std::time::Instant,
    observer: Option<(guard::NetworkGuardPublication, Result<(), String>)>,
    initial: Option<GuardInitial>,
    probe: Option<std::sync::Arc<guard_probe::Local>>,
}

#[derive(Debug)]
struct GuardInitial {
    owner: crate::network_replay::NetworkStreamOwner,
    // Keep the enrolled native task pinned across the initial exec's MM change.
    task: OwnedFd,
    result: Result<(), String>,
}

impl RuntimeShared {
    fn retain_completed_collections(
        self: &std::sync::Arc<Self>,
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
        self.retain_original_connects(controller)?;
        controller.retain_native_birth_cleanups()
    }
}

impl NetworkRuntimeOwner {
    /// Join original-socket workers before disposing their Tokio runtime.
    /// Failure retains the worker, raw result and original absolute deadline in
    /// the existing outside recovery owner; it is not transport settlement.
    ///
    /// # Safety
    /// The backend future has ended. Independently spawned callbacks may still
    /// finish; native admission is atomically closed before the worker snapshot.
    /// The authenticated startup owner must remain alive through container teardown.
    pub async unsafe fn finish_native_controller_tasks(&mut self) -> std::io::Result<()> {
        let deadline = {
            let mut original = self.shared.transport_terminal_deadline.lock().unwrap();
            *original.get_or_insert_with(|| {
                std::time::Instant::now() + std::time::Duration::from_secs(1)
            })
        };
        self.shared.native_workers.lock().unwrap().closed = true;
        let original = self.shared.finish_original_connects(deadline).await;
        let workers = self.shared.finish_native_after_backend(deadline).await;
        let settled = self.shared.native_streams.lock().unwrap().settled();
        // A failed backend never reaches finish_accepted_transport. Its driver
        // thread would outlive the container callback's thread-only exit and
        // keep this controller process, its endpoint and the provider alive
        // until outside cancellation. A joined driver returns its first outcome.
        let controller = self.shared.controller.lock().unwrap().clone();
        let driver = match controller {
            Some(Ok(controller)) => {
                // A replied observation can be transport-quiescent while the
                // service still retains its final journal/creation receipt.
                // Keep the same driver alive for that existing handshake and
                // charge it to the deadline already used by native cleanup.
                let observations = tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    self.shared.finish_observations_after_backend(&controller),
                )
                .await
                .map_err(|_| std::io::Error::other("accepted terminal transport deadline"))
                .and_then(|result| result);
                let driver = self.shared.stop_resolved_driver(
                    &controller,
                    deadline,
                    original.is_err() || observations.is_err(),
                );
                observations.and(driver)
            }
            None | Some(Err(_)) => Ok(()),
        };
        original.and(workers).and(settled).and(driver)
    }

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
        let controller = self.shared.controller.lock().unwrap().clone();
        let controller = match controller {
            None => {
                let primary = (|| {
                    if self.shared.accepted.lock().unwrap().collections_settled()?
                        && self.shared.physical.lock().unwrap().enrollments_settled()
                    {
                        Ok(())
                    } else {
                        Err(std::io::Error::other(
                            "accepted collection has no transport owner",
                        ))
                    }
                })();
                let workers = self.shared.finish_native_after_backend(deadline).await;
                let settled = self.shared.native_streams.lock().unwrap().settled();
                return primary.and(workers).and(settled);
            }
            Some(Ok(controller)) => controller,
            Some(Err(error)) => {
                let workers = self.shared.finish_native_after_backend(deadline).await;
                let settled = self.shared.native_streams.lock().unwrap().settled();
                return Err::<(), _>(std::io::Error::other(error))
                    .and(workers)
                    .and(settled);
            }
        };
        let operation = async {
            self.shared
                .finish_observations_after_backend(&controller)
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
                || !self.shared.physical.lock().unwrap().enrollments_settled()
                || !controller.quiescent()?
            {
                return Err(std::io::Error::other(
                    "accepted terminal requests remain unresolved",
                ));
            }
            Ok(())
        };
        let primary = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), operation)
            .await
            .map_err(|_| std::io::Error::other("accepted terminal transport deadline"))
            .and_then(|result| result);
        // A semantic-journal error is still the primary RED. It cannot prevent
        // independently authorized dead-call retirement under the same clock.
        self.shared.native_workers.lock().unwrap().closed = true;
        let original = self.shared.finish_original_connects(deadline).await;
        // An admitted capture can still submit its original provider request.
        // Keep transport progress alive through actual native-worker completion.
        let workers = self.shared.finish_native_after_backend(deadline).await;
        let driver = self.shared.stop_resolved_driver(
            &controller,
            deadline,
            primary.is_err() || original.is_err(),
        );
        let settled = self.shared.native_streams.lock().unwrap().settled();
        primary.and(original).and(driver).and(workers).and(settled)
    }
}

impl RuntimeShared {
    // Both normal GlobalState cleanup and failed-backend disposal must retire
    // the final observation using its retained authenticated origin. The
    // caller owns the existing absolute terminal deadline around both awaits.
    async fn finish_observations_after_backend(
        &self,
        controller: &accepted_controller::Controller,
    ) -> std::io::Result<()> {
        self.fd_journal
            .lock()
            .await
            .finish_after_backend(controller)
            .await?;
        self.creations
            .lock()
            .await
            .finish_after_backend(controller)
            .await
    }

    /// Stop the native driver only when no terminal request can still need it.
    fn stop_resolved_driver(
        &self,
        controller: &accepted_controller::Controller,
        deadline: std::time::Instant,
        unresolved: bool,
    ) -> std::io::Result<()> {
        // stop_and_join sets the stop latch even when its deadline expired.
        // A timeout or semantic error is not proof that required requests
        // drained: retain this same driver to receive their eventual replies.
        // Recheck after workers: a late capture may have installed an original
        // Call after finish_original_connects observed an empty Calls set.
        if unresolved
            || !self.native_workers_terminated()
            || !self.native_streams.lock().unwrap().originals().is_empty()
            || !controller.quiescent()?
        {
            return Err(std::io::Error::other(
                "accepted native driver retained for unresolved terminal requests",
            ));
        }
        match self.driver.lock().unwrap().as_mut() {
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
    /// Retain the same run owner outside its backend future and Tokio runtime.
    /// This clones custody, not a task, FD, admission or operation authority.
    #[doc(hidden)]
    pub fn controller_disposal_owner(&self) -> NetworkRuntimeOwner {
        NetworkRuntimeOwner {
            shared: self.shared.clone(),
        }
    }

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
        shared.copy_wire = Some(ProviderWireFormat::Abi7Copy4);
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
                copy_wire: None,
                physical: Mutex::default(),
                accepted: Mutex::new(accepts),
                listeners: Mutex::default(),
                creations: tokio::sync::Mutex::default(),
                fd_journal: tokio::sync::Mutex::default(),
                native_streams: Mutex::default(),
                native_workers: Mutex::default(),
                native_terminal_failure: Mutex::default(),
            }),
            drops: None,
            controlled_private_drain: Mutex::default(),
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
    /// nonblocking and CLOEXEC. The wire format must be the exact launch artifact
    /// authenticated by the parent READY barrier before workload permission.
    /// Guest exec must preserve CLOEXEC: a raw numeric
    /// pre-exec close is not used because a prior Command hook can reuse that slot.
    pub unsafe fn from_authenticated_startup(
        endpoint: OwnedFd,
        incarnation: [u8; 16],
        wire_format: ProviderWireFormat,
    ) -> (NetworkRuntimeOwner, Self) {
        let shared = std::sync::Arc::new(RuntimeShared {
            guard: Mutex::default(),
            endpoint: Some(endpoint),
            controller: Mutex::default(),
            driver: Mutex::default(),
            transport_terminal_deadline: Mutex::default(),
            incarnation,
            copy_wire: Some(wire_format),
            physical: Mutex::default(),
            accepted: Mutex::default(),
            listeners: Mutex::default(),
            creations: tokio::sync::Mutex::default(),
            fd_journal: tokio::sync::Mutex::default(),
            native_streams: Mutex::default(),
            native_workers: Mutex::default(),
            native_terminal_failure: Mutex::default(),
        });
        (
            NetworkRuntimeOwner {
                shared: shared.clone(),
            },
            Self {
                shared,
                #[cfg(test)]
                drops: None,
                #[cfg(test)]
                controlled_private_drain: Mutex::default(),
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
                probe: None,
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
            copy_wire: None,
            physical: Mutex::default(),
            accepted: Mutex::default(),
            listeners: Mutex::default(),
            creations: tokio::sync::Mutex::default(),
            fd_journal: tokio::sync::Mutex::default(),
            native_streams: Mutex::default(),
            native_workers: Mutex::default(),
            native_terminal_failure: Mutex::default(),
        });
        (
            NetworkRuntimeOwner {
                shared: shared.clone(),
            },
            Self {
                shared,
                #[cfg(test)]
                drops: None,
                #[cfg(test)]
                controlled_private_drain: Mutex::default(),
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
            probe: None,
            control,
            deadline,
            observer: None,
            initial: None,
        });
        Ok(())
    }

    /// Start one independently owned native observer before guest permission.
    /// A partial start is retained and never retried with a new abort token.
    fn start_guard_observer(
        guard: &mut GuardRuntime,
    ) -> std::io::Result<guard::NetworkGuardPublication> {
        if let Some((publication, result)) = &guard.observer {
            return result
                .clone()
                .map(|()| publication.clone())
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
        result.map(|()| publication).map_err(std::io::Error::other)
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
        let physical = self.shared.physical.lock().unwrap();
        let task = physical.get(owner)?;
        if let Some(initial) = &guard.initial {
            // Exec replaces the MM, not the native guard's task-storage entry.
            // Only the globally consumed initial EXEC receipt may bridge that
            // transition, and both custody pins must name the enrolled task.
            // https://github.com/rrnewton/hermit/pull/3464
            let same_exec = physical.initial_exec(owner)?.is_some_and(|receipt| {
                receipt.caller == initial.owner.thread
                    && receipt.process == initial.owner.thread
                    && receipt.mm == initial.owner.mm
                    && receipt.mm.for_exec(receipt.process) == owner.mm
            });
            if initial.owner.thread != owner.thread
                || (initial.owner.mm != owner.mm && !same_exec)
                || PidfdIdentity::read(&initial.task)? != PidfdIdentity::read(task)?
            {
                return Err(std::io::Error::other("guard initial task changed"));
            }
            return initial.result.clone().map_err(std::io::Error::other);
        }
        let task = task.try_clone()?;
        drop(physical);
        guard.initial = Some(GuardInitial {
            owner,
            task,
            result: Err("guard initial enrollment submitted without result".into()),
        });
        let result = unsafe {
            guard.control.register_stopped_initial(
                guard.initial.as_ref().unwrap().task.as_fd(),
                guard.deadline,
            )
        }
        .and_then(|()| {
            // The native observer requires completed initial enrollment. Keep
            // this one admission pending until both operations have succeeded;
            // the stopped guest cannot receive permission between them.
            Self::start_guard_observer(guard)?;
            if std::time::Instant::now() >= guard.deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "guard initial observer completion exceeded original startup deadline",
                ));
            }
            if !guard
                .observer
                .as_ref()
                .is_some_and(|(_, result)| result.is_ok())
            {
                return Err(std::io::Error::other(
                    "guard initial task precedes actual observer startup",
                ));
            }
            Ok(())
        })
        .map_err(|error| error.to_string());
        guard.initial.as_mut().unwrap().result = result.clone();
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
                    accepted_controller::Controller::from_startup(
                        endpoint,
                        self.shared.incarnation,
                        self.shared.copy_wire.ok_or_else(|| {
                            std::io::Error::other(
                                "accepted runtime lacks authenticated wire format",
                            )
                        })?,
                    )
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
        // Listener pins introduce another physical alias owner. The first
        // foreground capability covers only runs which never entered this
        // ownership path; map readiness/emptiness is not its authority.
        self.revoke_foreground_lineage();
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
                    .confirm_resolved(owner, lease, matched)?;
                let end = self
                    .shared
                    .accepted
                    .lock()
                    .unwrap()
                    .installation_end(owner, lease)?;
                let mut journal = self.shared.fd_journal.lock().await;
                journal.through(&controller, owner, end).await?;
                self.shared.accepted.lock().unwrap().retain_historical(
                    owner,
                    lease,
                    journal.history(),
                )?;
                // Historical validation does not issue current-slot authority.
                Ok(matched)
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
        self.shared.accepted.lock().unwrap().retain_submission(
            owner,
            lease,
            accepted::SubmittedAccept {
                listener: physical,
                fd,
                flags,
            },
        )?;
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

    /// Consume a clone of the exact task description acquired by the stopped
    /// callback under scheduler identity admission. No numeric lookup occurs
    /// here or when the provider/guard receives subsequent custody rights.
    pub(crate) fn register_ptrace_task(
        &self,
        task: crate::scheduler::RetainedPhysicalThread,
    ) -> std::io::Result<()> {
        let (owner, process, thread, pin) = task.into_parts();
        self.shared
            .physical
            .lock()
            .unwrap()
            .register(owner, process, thread, || Ok(pin))
    }

    pub(crate) fn has_initial_table_provider(&self) -> bool {
        self.shared.endpoint.is_some()
    }

    pub(crate) fn bind_initial_exec(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        receipt: crate::types::ExecFilesReceipt,
    ) -> std::io::Result<()> {
        self.shared
            .physical
            .lock()
            .unwrap()
            .bind_initial_exec(owner, receipt)
    }

    /// Initial scheduler-root registration only. Child shared/copy associations
    /// require the original clone permit and remain a separate prerequisite.
    /// Unknown/canceled preparation stays in this same run-owned task entry.
    pub(crate) async fn prepare_initial_table(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
    ) -> std::io::Result<Option<InitialTableTicket>> {
        if self.shared.endpoint.is_none() {
            return Ok(None);
        }
        let controller = self.accepted_controller()?;
        let sequence = {
            let mut tasks = self.shared.physical.lock().unwrap();
            let registration = tasks.begin_initial(owner)?;
            match tasks.preparation(owner)? {
                Some(prior) => prior.map_err(std::io::Error::other)?,
                None => {
                    let submitted = controller
                        .prepare(
                            accepted_controller::Effect::PrepareTableEnrollment(registration),
                            owner,
                            &accepted_provider::Request::PrepareTableEnrollment {
                                registration,
                                mm: owner.mm.generation(),
                                expected_table: 0,
                            },
                            || Ok(vec![tasks.get(owner)?.as_fd().try_clone_to_owned()?]),
                        )
                        .map_err(|e| e.to_string());
                    tasks.retain_preparation(owner, submitted.clone())?;
                    submitted.map_err(std::io::Error::other)?
                }
            }
        };
        match controller.response(sequence).await? {
            accepted_provider::Reply::Prepared(observation)
                if observation.status.returned == 0 && observation.raw != 0 =>
            {
                self.shared
                    .physical
                    .lock()
                    .unwrap()
                    .prepared(owner, sequence, observation.raw)
                    .map(Some)
            }
            _ => Err(std::io::Error::other(
                "initial table preparation failed; original response retained",
            )),
        }
    }

    pub(crate) async fn collect_initial_table(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        ticket: InitialTableTicket,
        register_read_succeeded: bool,
    ) -> std::io::Result<InitialTableAssociation> {
        let controller = self.accepted_controller()?;
        let sequence = {
            let mut tasks = self.shared.physical.lock().unwrap();
            tasks.native_read(owner, ticket, register_read_succeeded)?;
            match tasks.collection(owner)? {
                Some(prior) => prior.map_err(std::io::Error::other)?,
                None => {
                    let submitted = controller
                        .prepare(
                            accepted_controller::Effect::CollectTableEnrollment(
                                ticket.registration,
                            ),
                            owner,
                            &accepted_provider::Request::CollectTableEnrollment {
                                command: ticket.command,
                                prepared_request: ticket.prepared_request,
                            },
                            || Ok(vec![]),
                        )
                        .map_err(|e| e.to_string());
                    tasks.retain_collection(owner, submitted.clone())?;
                    submitted.map_err(std::io::Error::other)?
                }
            }
        };
        let observed = match controller.response(sequence).await {
            Ok(accepted_provider::Reply::TableEnrollmentEffect(observation)) => Ok(observation),
            Ok(_) => Err("initial table collection changed response kind".into()),
            Err(error) => Err(error.to_string()),
        };
        self.shared
            .physical
            .lock()
            .unwrap()
            .retain_raw(owner, observed.clone())?;
        let observed = observed.map_err(std::io::Error::other)?;
        if observed.status.returned != 0 || observed.raw.enrollment.end == 0 {
            return Err(std::io::Error::other(
                "initial census collection failed; raw completion retained",
            ));
        }
        // Bind the raw kernel identity through the exact retained pidfd/request,
        // before joining journal rows. Guest/DetTid IDs are namespace-local.
        let binding = self
            .shared
            .physical
            .lock()
            .unwrap()
            .collected_binding(owner, ticket, sequence)?;
        let association = {
            let mut journal = self.shared.fd_journal.lock().await;
            journal
                .through(&controller, owner, observed.raw.enrollment.end)
                .await?;
            journal.history().enrollment(&binding)?
        };
        self.shared
            .physical
            .lock()
            .unwrap()
            .complete(owner, association.clone())?;
        Ok(association)
    }

    /// Called only from the initial EXEC collection path before any resume.
    /// The census attests a frozen table with one task reference. The exact
    /// retained PIDFD and that exclusion bind each numeric duplicate to its row.
    pub(crate) async fn observe_initial_metadata(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
    ) -> std::io::Result<Vec<InitialFileStat>> {
        self.observe_initial_metadata_with(owner, |task, process, view| {
            use std::os::unix::fs::MetadataExt;
            // fstat runs in this controller, not in the outside provider service.
            // Compare actual user namespace objects while the retained child is
            // stopped and unreaped; PID reuse cannot substitute another task.
            let local = std::fs::metadata("/proc/thread-self/ns/user")?;
            let target = std::fs::metadata(format!("/proc/{process}/ns/user"))?;
            if (local.dev(), local.ino()) != (target.dev(), target.ino()) {
                return Err(std::io::Error::other(
                    "initial stat observer has a different user namespace",
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            let mut captured = Vec::new();
            for row in &view.descriptors {
                if !seen.insert(row.physical_file) {
                    continue;
                }
                let held = match capture_socket(task, row.fd) {
                    Ok(held) => held,
                    Err(error) => {
                        captured.push(physical::InitialFileCapture {
                            fd: row.fd,
                            physical_file: row.physical_file,
                            capture: accepted_provider::CallStatus {
                                operation: "initial pidfd_getfd".into(),
                                returned: -1,
                                errno: error.raw_os_error(),
                            },
                            metadata: Err(error.to_string()),
                            release: None,
                        });
                        break;
                    }
                };
                let capture = accepted_provider::CallStatus {
                    operation: "initial pidfd_getfd".into(),
                    returned: held.as_raw_fd(),
                    errno: None,
                };
                let metadata = required_initial_metadata(
                    installation_observation::held_file_profile(held.as_fd()).map_err(|error| {
                        reverie::syscalls::Errno::from_ret(
                            (-i64::from(error.raw_os_error().unwrap_or(libc::EIO))) as usize,
                        )
                        .err()
                        .unwrap_or(reverie::syscalls::Errno::EIO)
                    }),
                    row.fd,
                    row.physical_file,
                )
                .map_err(|error| error.to_string());
                let rc = unsafe { libc::close(held.into_raw_fd()) };
                let release = accepted_provider::CallStatus {
                    operation: "initial auxiliary close".into(),
                    returned: rc,
                    errno: (rc < 0).then(|| {
                        std::io::Error::last_os_error()
                            .raw_os_error()
                            .unwrap_or(libc::EIO)
                    }),
                };
                let failed = metadata.is_err() || rc != 0;
                captured.push(physical::InitialFileCapture {
                    fd: row.fd,
                    physical_file: row.physical_file,
                    capture,
                    metadata,
                    release: Some(release),
                });
                if failed {
                    break;
                }
            }
            Ok(captured)
        })
        .await
    }

    async fn observe_initial_metadata_with(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        capture: impl FnOnce(
            &OwnedFd,
            i32,
            &InitialTableView,
        ) -> std::io::Result<Vec<physical::InitialFileCapture>>
        + Send
        + 'static,
    ) -> std::io::Result<Vec<InitialFileStat>> {
        // No suspension lies between marking the enrollment and retaining the
        // submitted worker; cancellation cannot strand an unowned pending job.
        let executor = tokio::runtime::Handle::try_current().map_err(std::io::Error::other)?;
        let work = match self
            .shared
            .physical
            .lock()
            .unwrap()
            .begin_initial_metadata(owner, |task| task.as_fd().try_clone_to_owned())?
        {
            physical::InitialMetadataRequest::Ready(metadata) => return Ok(metadata),
            physical::InitialMetadataRequest::Work(work) => work,
        };
        let registration = work.registration;
        let association = work.association.clone();
        let shared = std::sync::Arc::clone(&self.shared);
        let submitted = self.shared.start_native_worker(executor, move || {
            // FUSE getattr and the final auxiliary close may block. This
            // existing run-owned worker holds no physical/scheduler mutex.
            let result = capture(&work.task, work.process, &work.association.view())
                .map_err(|error| error.to_string());
            let mut physical = shared.physical.lock().unwrap();
            physical.retain_initial_metadata(
                owner,
                work.registration,
                &work.association,
                result,
            )?;
            physical.initial_metadata(owner)
        });
        let (worker, receive) = match submitted {
            Ok(submitted) => submitted,
            Err(error) => {
                self.shared
                    .physical
                    .lock()
                    .unwrap()
                    .retain_initial_metadata(
                        owner,
                        registration,
                        &association,
                        Err(error.to_string()),
                    )?;
                return Err(error);
            }
        };
        let result = receive.await;
        self.shared.join_native_worker(&worker).await?;
        result.map_err(std::io::Error::other)?
    }

    pub(crate) fn initial_metadata_identity(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
    ) -> std::io::Result<Option<physical::InitialMetadataIdentity>> {
        self.shared
            .physical
            .lock()
            .unwrap()
            .initial_metadata_identity(owner)
    }

    pub(crate) fn admit_initial_table(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        claim: InitialTableClaim,
        admit: impl FnOnce(
            &InitialTableAssociation,
            &InitialTableClaim,
            &OwnedFd,
            Option<&Result<(), String>>,
        ) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.shared
            .physical
            .lock()
            .unwrap()
            .admit_semantics(owner, claim, admit)
    }

    pub(crate) fn forget_task(&self, owner: crate::network_replay::NetworkStreamOwner) {
        self.shared.accepted.lock().unwrap().abandon(owner);
        self.shared.physical.lock().unwrap().forget(owner);
    }
    pub(crate) fn forget_shared_task(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
    ) -> std::io::Result<()> {
        self.shared.accepted.lock().unwrap().abandon(owner);
        self.shared
            .physical
            .lock()
            .unwrap()
            .forget_shared_child(owner)
    }

    /// Called only after both original-connect and stream final observations.
    /// The scheduler lock retains the exact historical projection throughout.
    pub(crate) fn finish_shared_terminal_observations(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        projection: &std::sync::Arc<native_birth_outcome::NativeTaskProjection>,
    ) -> std::io::Result<()> {
        self.shared
            .physical
            .lock()
            .unwrap()
            .finish_shared_final_observations(owner, projection)
    }

    pub(crate) fn submit_accept(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkAcceptLeaseId,
        call: crate::network_replay::NetworkStreamCallId,
    ) -> std::io::Result<()> {
        self.revoke_foreground_lineage();
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

    fn start_native_worker<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> std::io::Result<T> + Send + 'static,
    ) -> std::io::Result<(
        NativeWorkerHandle,
        tokio::sync::oneshot::Receiver<std::io::Result<T>>,
    )> {
        self.shared.start_native_worker(
            tokio::runtime::Handle::try_current().map_err(std::io::Error::other)?,
            operation,
        )
    }

    async fn run_native_worker<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> std::io::Result<T> + Send + 'static,
    ) -> std::io::Result<T> {
        let (worker, receive) = self.start_native_worker(operation)?;
        let result = receive.await;
        self.shared.join_native_worker(&worker).await?;
        result.map_err(std::io::Error::other)?
    }

    /// A bounded caller has expired, not completed its outstanding workers.
    /// Preserve their actual handles and late results for terminal join; close
    /// further admission and retain RED even if the physical work later ends.
    pub(crate) fn refuse_native_poll_deadline(&self) -> std::io::Error {
        let message = "native poll exceeded its original one-second helper deadline; custody retained";
        let mut owned = self.shared.native_workers.lock().unwrap();
        owned.closed = true;
        for worker in &owned.tasks {
            worker.deadline_failure.lock().unwrap().get_or_insert_with(|| message.to_owned());
        }
        self.shared.native_terminal_failure.lock().unwrap().get_or_insert_with(|| message.to_owned());
        std::io::Error::other(message)
    }

    /// Test-only physical capture premise: the real TCP object is already
    /// owned, but no provider or pidfd selection is claimed by this fixture.
    #[cfg(test)]
    pub(crate) fn controlled_capture_poll_pin(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        pin: OwnedFd,
    ) -> std::io::Result<()> {
        self.shared.native_streams.lock().unwrap().capture(owner, call, pin)
    }

    async fn run_native_release_worker<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> std::io::Result<T> + Send + 'static,
    ) -> std::io::Result<T> {
        let (worker, receive) = self.shared.start_native_release_worker(
            tokio::runtime::Handle::try_current().map_err(std::io::Error::other)?,
            operation,
        )?;
        let result = receive.await;
        self.shared.join_native_worker(&worker).await?;
        result.map_err(std::io::Error::other)?
    }

    /// Clone the exact scheduler-registered task authority before admission can
    /// outlive its callback. Owner exit may remove the registry entry afterward.
    pub(crate) fn prepare_native_capture_task(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
    ) -> std::io::Result<OwnedFd> {
        self.shared
            .physical
            .lock()
            .unwrap()
            .get(owner)?
            .as_fd()
            .try_clone_to_owned()
    }

    pub(crate) fn queue_native_capture_recovery(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        recovery: NativeCaptureRecovery,
    ) -> std::io::Result<()> {
        let calls = recovery
            .engine
            .lock()
            .unwrap()
            .abandoned_native_captures(owner);
        if calls.is_empty() {
            return Ok(());
        }
        let shared = self.shared.clone();
        // This is the existing run-owned worker registry. Discarding the reply
        // does not discard its join handle, deadline failure, or physical result.
        let _ = self.start_native_worker(move || {
            for call in calls {
                recovery.retire_known(&shared, owner, call)?;
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Close/consume only a known capture through the existing run-owned worker.
    /// False keeps the local admission retained; absence is never no-submission.
    pub(crate) async fn retire_failed_receive_admission(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        recovery: NativeCaptureRecovery,
    ) -> std::io::Result<bool> {
        recovery
            .engine
            .lock()
            .unwrap()
            .abandon_failed_native_receive_admission(owner, call)
            .map_err(std::io::Error::other)?;
        let shared = self.shared.clone();
        self.run_native_worker(move || recovery.retire_known(&shared, owner, call))
            .await
    }

    /// The caller owns the engine's PinAcquireSubmitted call and the same short
    /// FD/OFD admission as BeginStreamCall. The registered PIDFD_THREAD selects
    /// the stopped task's table; a numeric descriptor alone is not authority.
    pub(crate) async fn capture_native_stream(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        fd: i32,
        task: OwnedFd,
        identity: original_installation::FileIdentity,
        recovery: NativeCaptureRecovery,
    ) -> std::io::Result<crate::network_replay::NetworkStreamPinOutcome> {
        self.capture_native_stream_with_identity(
            owner,
            call,
            move || capture_socket(&task, fd),
            Some(identity),
            recovery,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn capture_native_stream_with(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        capture: impl FnOnce() -> std::io::Result<OwnedFd> + Send + 'static,
        recovery: NativeCaptureRecovery,
    ) -> std::io::Result<crate::network_replay::NetworkStreamPinOutcome> {
        self.capture_native_stream_with_identity(owner, call, capture, None, recovery)
            .await
    }

    async fn capture_native_stream_with_identity(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        capture: impl FnOnce() -> std::io::Result<OwnedFd> + Send + 'static,
        identity: Option<original_installation::FileIdentity>,
        recovery: NativeCaptureRecovery,
    ) -> std::io::Result<crate::network_replay::NetworkStreamPinOutcome> {
        let shared = self.shared.clone();
        self.run_native_worker(move || {
            let result = (|| {
                let pin = match capture() {
                    Ok(pin) => pin,
                    Err(error) => {
                        // The production closure is exactly pidfd_getfd. An
                        // internal failure without Linux errno is UNKNOWN.
                        let errno = error.raw_os_error().ok_or(error)?;
                        shared
                            .native_streams
                            .lock()
                            .unwrap()
                            .capture_failed(owner, call, errno)?;
                        return Ok(crate::network_replay::NetworkStreamPinOutcome::Failed(
                            errno,
                        ));
                    }
                };
                let flags = unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFD) };
                let error = (flags < 0).then(std::io::Error::last_os_error);
                {
                    let mut calls = shared.native_streams.lock().unwrap();
                    match identity {
                        Some(identity) => {
                            calls.capture_authenticated(owner, call, pin, identity)?
                        }
                        None => calls.capture(owner, call, pin)?,
                    }
                    calls.retain_capture_publication(owner, call, recovery.clone())?;
                }
                if let Some(error) = error {
                    return Err(error);
                }
                if flags & libc::FD_CLOEXEC == 0 {
                    return Err(std::io::Error::other("native stream capture lacks CLOEXEC"));
                }
                Ok(crate::network_replay::NetworkStreamPinOutcome::Acquired)
            })();
            // This executes even when the RPC receiver has been canceled. A
            // known acquisition is still distinct from a known physical close.
            let retired = recovery.retire_known(&shared, owner, call);
            match (result, retired) {
                (Ok(value), Ok(_)) => Ok(value),
                (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
                (Err(primary), Err(cleanup)) => Err(std::io::Error::other(format!(
                    "{primary}; capture retirement: {cleanup}"
                ))),
            }
        })
        .await
    }

    pub(crate) fn finish_native_capture_failure(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        errno: i32,
    ) -> std::io::Result<()> {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .finish_failed_capture(owner, call, errno)
    }

    pub(crate) fn bind_native_stream_lease(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        lease: crate::network_replay::NetworkStreamLeaseId,
    ) -> std::io::Result<()> {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .bind_lease(owner, call, lease)
    }

    pub(crate) async fn execute_native_stream(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkStreamLeaseId,
        effect: crate::network_replay::NetworkStreamPhysicalEffect,
    ) -> std::io::Result<native_peer::Observation> {
        if matches!(
            effect,
            crate::network_replay::NetworkStreamPhysicalEffect::Drain { .. }
                | crate::network_replay::NetworkStreamPhysicalEffect::Peek { .. }
        ) {
            return self
                .shared
                .execute_helper_receive(owner, lease, effect)
                .await;
        }
        let shared = self.shared.clone();
        self.run_native_worker(move || {
            let work =
                shared
                    .native_streams
                    .lock()
                    .unwrap()
                    .prepare(owner, lease, effect.clone())?;
            let result = work.perform_control()?;
            shared
                .native_streams
                .lock()
                .unwrap()
                .retain(owner, lease, &effect, result.clone())?;
            Ok(result)
        })
        .await
    }

    pub(crate) fn preflight_native_stream(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkStreamLeaseId,
        effect: &crate::network_replay::NetworkStreamPhysicalEffect,
        observed: &native_peer::Observation,
    ) -> std::io::Result<()> {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .preflight_confirmation(owner, lease, effect, observed)
    }

    pub(crate) fn confirm_native_stream(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkStreamLeaseId,
        effect: &crate::network_replay::NetworkStreamPhysicalEffect,
        observed: &native_peer::Observation,
    ) -> std::io::Result<()> {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .confirm(owner, lease, effect, observed)
    }

    pub(crate) fn check_native_probe_bytes(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkStreamLeaseId,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .check_probe_bytes(owner, lease, bytes)
    }

    pub(crate) fn finish_native_stream_lease(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        lease: crate::network_replay::NetworkStreamLeaseId,
    ) -> std::io::Result<()> {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .finish_lease(owner, lease)
    }

    pub(crate) fn abort_native_poll_lease(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
        lease: crate::network_replay::NetworkStreamLeaseId,
    ) -> std::io::Result<()> {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .abort_poll_lease(owner, call, lease)
    }

    pub(crate) async fn release_native_stream(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
    ) -> std::io::Result<()> {
        let shared = self.shared.clone();
        // close may wait for SO_LINGER. Keep that wait off the scheduler thread
        // and do not hold the runtime registry mutex while it waits.
        self.run_native_release_worker(move || {
            // Move the actual pin only after worker admission. A rejected late
            // callback must leave it in run custody, not close it while dropping
            // a never-submitted closure on the current-thread executor.
            let work = shared
                .native_streams
                .lock()
                .unwrap()
                .prepare_release(owner, call)?;
            let release = work.perform();
            shared
                .native_streams
                .lock()
                .unwrap()
                .retain_release(owner, call, release)
        })
        .await
    }

    pub(crate) fn finish_native_stream_release(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
    ) -> std::io::Result<()> {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .finish_release(owner, call)
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::*;

    include!("network_runtime/failed_backend_retirement.rs");

    mod guard_initial_exec_tests {
        include!("network_runtime/guard_initial_exec_tests.rs");
    }

    #[derive(Debug, Default)]
    struct AdmissionControl {
        calls: Mutex<Vec<&'static str>>,
        fail_register: bool,
        fail_observer: bool,
        delay_observer_past_deadline: bool,
        registration_deadline: Mutex<Option<std::time::Instant>>,
        observer_gate: Option<(
            std::sync::mpsc::Sender<()>,
            Mutex<std::sync::mpsc::Receiver<()>>,
        )>,
        publication: Mutex<Option<guard::NetworkGuardPublication>>,
    }
    impl guard::NetworkGuardControl for AdmissionControl {
        unsafe fn register_stopped_initial(
            &self,
            pidfd: BorrowedFd<'_>,
            deadline: std::time::Instant,
        ) -> std::io::Result<()> {
            assert!(std::time::Instant::now() < deadline);
            *self.registration_deadline.lock().unwrap() = Some(deadline);
            let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))?;
            assert!(info.lines().any(|line| line.starts_with("Pid:")));
            let mut calls = self.calls.lock().unwrap();
            assert!(
                calls.is_empty(),
                "enrollment was retried or observer ran first"
            );
            calls.push("register");
            if self.fail_register {
                Err(std::io::Error::other("controlled enrollment failure"))
            } else {
                Ok(())
            }
        }
        fn observation(&self) -> guard::NetworkGuardOutcome {
            guard::NetworkGuardOutcome::Running
        }
        fn start_observer(&self, abort: guard::NetworkGuardControllerAbort) -> std::io::Result<()> {
            {
                let mut calls = self.calls.lock().unwrap();
                assert_eq!(*calls, ["register"]);
                calls.push("observer");
            }
            *self.publication.lock().unwrap() = Some(abort.publication());
            if self.delay_observer_past_deadline {
                let deadline = self.registration_deadline.lock().unwrap().unwrap();
                while std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            if let Some((entered, release)) = &self.observer_gate {
                entered.send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .unwrap();
            }
            if self.fail_observer {
                Err(std::io::Error::other("controlled observer failure"))
            } else {
                Ok(())
            }
        }
    }
    fn admission_runtime(
        control: Arc<AdmissionControl>,
    ) -> (
        NetworkRuntimeOwner,
        NetworkRuntimeResources,
        crate::network_replay::NetworkStreamOwner,
    ) {
        let thread =
            crate::types::DetTid::from_raw(unsafe { libc::syscall(libc::SYS_gettid) as i32 });
        let who = crate::network_replay::NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let (owner, runtime) = unsafe {
            NetworkRuntimeResources::from_authenticated_guard(
                control,
                std::time::Instant::now() + std::time::Duration::from_secs(1),
            )
        };
        let mut scheduler = crate::scheduler::Scheduler::new(&crate::Config::default());
        let task = scheduler
            .register_stopped_ptrace_thread(who, std::process::id() as i32, thread.as_raw())
            .unwrap();
        runtime.register_ptrace_task(task).unwrap();
        (owner, runtime, who)
    }
    #[test]
    fn guard_initial_admission_enrolls_then_starts_once_before_permission() {
        let control = Arc::new(AdmissionControl::default());
        let (_owner, runtime, who) = admission_runtime(control.clone());
        runtime.register_guard_initial(who).unwrap();
        runtime.register_guard_initial(who).unwrap();
        assert_eq!(*control.calls.lock().unwrap(), ["register", "observer"]);
        let stale = crate::network_replay::NetworkStreamOwner {
            mm: who.mm.for_exec(who.thread),
            ..who
        };
        assert!(runtime.register_guard_initial(stale).is_err());
        assert_eq!(*control.calls.lock().unwrap(), ["register", "observer"]);
    }
    #[test]
    fn guard_initial_admission_retains_enrollment_failure_without_start_or_retry() {
        let control = Arc::new(AdmissionControl {
            fail_register: true,
            ..Default::default()
        });
        let (_owner, runtime, who) = admission_runtime(control.clone());
        for _ in 0..2 {
            assert_eq!(
                runtime.register_guard_initial(who).unwrap_err().to_string(),
                "controlled enrollment failure"
            );
        }
        assert_eq!(*control.calls.lock().unwrap(), ["register"]);
        assert!(control.publication.lock().unwrap().is_none());
    }
    #[test]
    fn guard_initial_admission_retains_observer_failure_without_permission_or_retry() {
        let control = Arc::new(AdmissionControl {
            fail_observer: true,
            ..Default::default()
        });
        let (_owner, runtime, who) = admission_runtime(control.clone());
        for _ in 0..2 {
            assert_eq!(
                runtime.register_guard_initial(who).unwrap_err().to_string(),
                "controlled observer failure"
            );
        }
        assert_eq!(*control.calls.lock().unwrap(), ["register", "observer"]);
        assert!(control.publication.lock().unwrap().is_some());
    }
    #[test]
    fn guard_initial_admission_waits_for_actual_observer_start_return() {
        let (entered, observation) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let control = Arc::new(AdmissionControl {
            observer_gate: Some((entered, Mutex::new(blocked))),
            ..Default::default()
        });
        let (_owner, runtime, who) = admission_runtime(control.clone());
        let (done, permission) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            done.send(runtime.register_guard_initial(who)).unwrap();
        });
        observation
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            permission.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        permission
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        assert_eq!(*control.calls.lock().unwrap(), ["register", "observer"]);
    }

    #[test]
    fn guard_initial_admission_refuses_late_observer_without_renewing_deadline() {
        let control = Arc::new(AdmissionControl {
            delay_observer_past_deadline: true,
            ..Default::default()
        });
        let (_owner, runtime, who) = admission_runtime(control.clone());
        let original = runtime
            .shared
            .guard
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .deadline;
        for _ in 0..2 {
            assert_eq!(
                runtime.register_guard_initial(who).unwrap_err().to_string(),
                "guard initial observer completion exceeded original startup deadline"
            );
        }
        assert_eq!(*control.calls.lock().unwrap(), ["register", "observer"]);
        assert_eq!(
            runtime
                .shared
                .guard
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .deadline,
            original
        );
        assert!(control.publication.lock().unwrap().is_some());
    }

    pub(super) fn fixture(incarnation: u8) -> (NetworkRuntimeResources, Arc<AtomicUsize>) {
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
                    copy_wire: None,
                    physical: Mutex::default(),
                    accepted: Mutex::default(),
                    listeners: Mutex::default(),
                    creations: tokio::sync::Mutex::default(),
                    fd_journal: tokio::sync::Mutex::default(),
                    native_streams: Mutex::default(),
                    native_workers: Mutex::default(),
                    native_terminal_failure: Mutex::default(),
                }),
                drops: Some(drops.clone()),
                controlled_private_drain: Mutex::default(),
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

    fn started_accepted_driver_fixture() -> (
        NetworkRuntimeOwner,
        NetworkRuntimeResources,
        std::sync::Arc<accepted_controller::Controller>,
        OwnedFd,
    ) {
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
        let (owner, runtime) = unsafe {
            NetworkRuntimeResources::from_authenticated_startup(
                OwnedFd::from_raw_fd(fds[0]),
                [93; 16],
                ProviderWireFormat::Abi8Copy5,
            )
        };
        let controller = runtime.accepted_controller().unwrap();
        (owner, runtime, controller, unsafe {
            OwnedFd::from_raw_fd(fds[1])
        })
    }

    fn accepted_driver_joined(owner: &NetworkRuntimeOwner) -> bool {
        match owner.shared.driver.lock().unwrap().as_ref() {
            Some(Ok(driver)) => driver.joined(),
            _ => panic!("accepted driver was not started"),
        }
    }

    // A failed backend skips GlobalState::clean_up, so the always-run native
    // cleanup is the only child-side step before the container callback's
    // thread-only exit. A live driver thread kept the controller process alive.
    #[tokio::test]
    async fn failed_backend_native_cleanup_joins_resolved_accepted_driver() {
        let (mut owner, _runtime, controller, _peer) = started_accepted_driver_fixture();
        assert!(!accepted_driver_joined(&owner));
        assert!(controller.quiescent().unwrap());
        unsafe { owner.finish_native_controller_tasks().await }.unwrap();
        assert!(accepted_driver_joined(&owner));
    }

    #[tokio::test]
    async fn failed_backend_native_cleanup_retains_driver_for_unresolved_request() {
        let (mut owner, _runtime, controller, _peer) = started_accepted_driver_fixture();
        let thread = crate::types::DetTid::from_raw(93);
        let who = crate::network_replay::NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        controller
            .prepare(
                accepted_controller::Effect::Observation(1),
                who,
                &accepted_provider::Request::ReadStatus,
                || Ok(vec![]),
            )
            .unwrap();
        let error = unsafe { owner.finish_native_controller_tasks().await }.unwrap_err();
        assert!(error.to_string().contains("retained for unresolved"));
        assert!(!accepted_driver_joined(&owner));
        owner
            .stop_collection_fixture(std::time::Instant::now() + std::time::Duration::from_secs(2));
    }

    fn admitted_capture_fixture() -> (
        crate::network_replay::NetworkReplayEngine,
        crate::network_replay::NetworkStreamOwner,
        crate::network_replay::NetworkStreamOwner,
        detcore_model::fd::FilesId,
        detcore_model::fd::OpenFileId,
        crate::network_replay::NetworkStreamCall,
        crate::network_replay::NetworkStreamLeaseId,
    ) {
        use chrono::TimeZone;
        use detcore_model::fd::FilesId;
        use detcore_model::fd::OpenFileId;

        use crate::network_replay::*;
        use crate::types::DetTid;
        use crate::types::FdSlot;
        use crate::types::FdSlotBinding;
        use crate::types::MmId;
        use crate::types::NetworkFdSlot;
        use crate::types::NetworkFdSlotReplacement;
        let thread = DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let sibling = NetworkStreamOwner {
            thread: DetTid::from_raw(62),
            mm: owner.mm,
        };
        let files = FilesId::initial(thread);
        let open_file = OpenFileId::new_socket(thread, 1);
        let mut engine =
            NetworkReplayEngine::record(chrono::Utc.timestamp_opt(1_790_000_000, 0).unwrap());
        engine.fd_table_fixture_enable();
        assert!(engine.register_initial_fd_table(owner, thread).unwrap());
        let NetworkFdMutationBegin::Admitted(admission) = engine
            .begin_fd_mutation(owner, files, NetworkFdMutationKind::Socket)
            .unwrap()
        else {
            panic!("socket admission")
        };
        let permit = admission.publication.permit;
        engine.submit_fd_mutation(owner, permit).unwrap();
        engine
            .confirm_fd_mutation_result(owner, permit, Ok(7))
            .unwrap();
        let binding = FdSlotBinding {
            slot: FdSlot { files, fd: 7 },
            generation: 1,
            open_file,
        };
        let replacement = NetworkFdSlotReplacement {
            files,
            installation_generation: 1,
            before: None,
            after: Some(NetworkFdSlot {
                binding,
                cloexec: false,
            }),
        };
        let effect = engine
            .confirm_fd_installation(owner, permit, replacement)
            .unwrap();
        let batch = NetworkFdPublicationBatch {
            files,
            sequence: admission.publication.acknowledged_sequence + 1,
            previous_generation: admission.publication.acknowledged_generation,
            through_generation: 1,
            entries: vec![NetworkFdPublicationEntry {
                replacement,
                effect,
            }],
        };
        assert_eq!(
            engine
                .publish_fd_publication(owner, permit, &batch)
                .unwrap(),
            batch
        );
        engine
            .acknowledge_fd_publication(owner, permit, &batch)
            .unwrap();
        assert_eq!(
            engine.fd_publication_fixture_register(sibling, Some(owner)),
            files
        );
        let control = engine
            .begin_socket_controls(owner, vec![open_file])
            .unwrap()[0]
            .1;
        let call = engine
            .begin_native_stream_call(owner, control, binding)
            .unwrap();
        (engine, owner, sibling, files, open_file, call, control)
    }

    // Exercise the production worker/retirement path with real owned socket
    // references and an actual raw Linux failure, without ptrace/BPF privilege.
    async fn abandoned_capture_control(success: bool, completion_first: bool, confirm_first: bool) {
        use std::io::Read;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;
        use std::time::Instant;

        use crate::network_replay::*;
        let (runtime, _) = fixture(34);
        let (engine, owner, sibling, files, open_file, call, _) = admitted_capture_fixture();
        let engine = Arc::new(Mutex::new(engine));
        let changed = Arc::new(tokio::sync::Notify::new());
        let recovery = NativeCaptureRecovery::new(engine.clone(), changed.clone(), |_| {});
        let (pin, mut peer) = if success {
            let (left, right) = UnixStream::pair().unwrap();
            right
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            (Some(OwnedFd::from(left)), Some(right))
        } else {
            (None, None)
        };
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let mut waiter = Box::pin(runtime.capture_native_stream_with(
            owner,
            call.id,
            move || {
                entered.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(1))
                    .map_err(std::io::Error::other)?;
                if let Some(pin) = pin {
                    Ok(pin)
                } else {
                    let raw = unsafe { libc::fcntl(-1, libc::F_DUPFD_CLOEXEC, 0) };
                    let error = std::io::Error::last_os_error();
                    assert_eq!(raw, -1);
                    Err(error)
                }
            },
            recovery.clone(),
        ));
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        tokio::time::timeout(Duration::from_secs(1), entered_rx)
            .await
            .unwrap()
            .unwrap();
        let original_worker = runtime.shared.native_workers.lock().unwrap().tasks[0].clone();
        // Drop the actual awaiting capture future, not merely a fake receiver.
        drop(waiter);
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(open_file),
            (1, 1, 1, 1)
        );
        if completion_first {
            release.send(()).unwrap();
            tokio::time::timeout(
                Duration::from_secs(1),
                runtime.shared.join_native_worker(&original_worker),
            )
            .await
            .unwrap()
            .unwrap();
            let raw = runtime
                .shared
                .native_streams
                .lock()
                .unwrap()
                .capture_outcome(owner, call.id)
                .unwrap();
            assert_eq!(raw, Some(if success { Ok(()) } else { Err(libc::EBADF) }));
            if confirm_first {
                assert!(success);
                engine
                    .lock()
                    .unwrap()
                    .confirm_stream_call_pin(owner, call.id, NetworkStreamPinOutcome::Acquired)
                    .unwrap();
            }
        }
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let mut engine = engine.lock().unwrap();
            engine.retire_fd_table_owner(owner);
            engine.stream_owner_gone(owner);
            assert_eq!(
                engine.native_capture_fixture_counts(open_file),
                (1, 1, if confirm_first { 0 } else { 1 }, 1)
            );
            if !confirm_first {
                assert!(matches!(
                    engine.begin_fd_mutation(sibling, files, NetworkFdMutationKind::Socket),
                    Err(NetworkReplayError::StreamOperationBusy(_))
                ));
            }
        }
        runtime
            .queue_native_capture_recovery(owner, recovery.clone())
            .unwrap();
        if !completion_first {
            // Let the owner-exit continuation observe UNKNOWN before permitting
            // the original physical worker to return a known result.
            let early = runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .last()
                .unwrap()
                .clone();
            tokio::time::timeout(
                Duration::from_secs(1),
                runtime.shared.join_native_worker(&early),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .native_capture_fixture_counts(open_file),
                (1, 1, 1, 1)
            );
            release.send(()).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), notified)
            .await
            .unwrap();
        runtime
            .shared
            .finish_native_workers(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        assert!(
            runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .is_empty()
        );
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .settled()
            .unwrap();
        if let Some(peer) = peer.as_mut() {
            // All original references were actually closed before retirement.
            assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
        }
        let mut engine = engine.lock().unwrap();
        assert_eq!(
            engine.native_capture_fixture_counts(open_file),
            (0, 0, 0, 0)
        );
        assert!(
            engine
                .confirm_stream_call_pin(
                    owner,
                    call.id,
                    if success {
                        NetworkStreamPinOutcome::Acquired
                    } else {
                        NetworkStreamPinOutcome::Failed(libc::EBADF)
                    }
                )
                .is_err()
        );
        for kind in [
            NetworkFdMutationKind::Socket,
            NetworkFdMutationKind::Clone {
                flags: reverie::syscalls::CloneFlags::CLONE_FILES,
            },
        ] {
            let NetworkFdMutationBegin::Admitted(next) =
                engine.begin_fd_mutation(sibling, files, kind).unwrap()
            else {
                panic!("surviving sibling must progress")
            };
            engine
                .native_capture_fixture_cancel_unsubmitted(sibling, next.publication.permit)
                .unwrap();
        }
        engine.retire_fd_table_owner(sibling);
        engine.stream_owner_gone(sibling);
        assert_eq!(
            engine.native_capture_fixture_counts(open_file),
            (0, 0, 0, 0)
        );
    }

    async fn ordinary_call_fixture() -> (
        NetworkRuntimeResources,
        Arc<Mutex<crate::network_replay::NetworkReplayEngine>>,
        crate::network_replay::NetworkStreamOwner,
        crate::network_replay::NetworkStreamCallId,
        detcore_model::fd::OpenFileId,
        std::os::unix::net::UnixStream,
        i32,
    ) {
        use crate::network_replay::*;
        let (runtime, _) = fixture(81);
        let (engine, owner, _, _, ofd, call, control) = admitted_capture_fixture();
        let engine = Arc::new(Mutex::new(engine));
        let recovery = NativeCaptureRecovery::new(
            engine.clone(),
            Arc::new(tokio::sync::Notify::new()),
            |_| {},
        );
        let (pin, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let fd = pin.as_raw_fd();
        assert_eq!(
            runtime
                .capture_native_stream_with(owner, call.id, move || Ok(pin.into()), recovery)
                .await
                .unwrap(),
            NetworkStreamPinOutcome::Acquired
        );
        {
            let mut engine = engine.lock().unwrap();
            engine
                .confirm_stream_call_pin(owner, call.id, NetworkStreamPinOutcome::Acquired)
                .unwrap();
            engine
                .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
                .unwrap();
        }
        (runtime, engine, owner, call.id, ofd, peer, fd)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_terminal_close_keeps_raw_or_unknown_effect_and_trace_failure() {
        use std::io::Read;

        use crate::network_replay::*;
        for raw_known in [false, true] {
            let (runtime, engine, owner, call, ofd, mut peer, _fd) = ordinary_call_fixture().await;
            let lease = engine
                .lock()
                .unwrap()
                .begin_stream_call_control(owner, call)
                .unwrap();
            runtime
                .bind_native_stream_lease(owner, call, lease)
                .unwrap();
            // Exercise the existing Calls physical primitive. No logical effect
            // confirmation is forged: the engine's control remains unresolved.
            if raw_known {
                let observed = runtime
                    .execute_native_stream(
                        owner,
                        lease,
                        NetworkStreamPhysicalEffect::ReadPeekOffset,
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    observed.confirmation,
                    NetworkStreamPhysicalResult::PeekOffset(-1)
                );
            } else {
                let shared = runtime.shared.clone();
                let failed: std::io::Result<()> = runtime
                    .run_native_worker(move || {
                        let _execution = shared.native_streams.lock().unwrap().prepare(
                            owner,
                            lease,
                            NetworkStreamPhysicalEffect::ReadPeekOffset,
                        )?;
                        panic!("forced worker loss before a retained physical result");
                    })
                    .await;
                assert!(failed.is_err());
            }
            {
                let mut engine = engine.lock().unwrap();
                engine.stream_owner_gone(owner);
                assert_eq!(engine.terminal_stream_admission(owner, call).unwrap(), None);
                assert!(engine.begin_stream_call_release(owner, call).is_err());
                let sibling = NetworkStreamOwner {
                    thread: crate::types::DetTid::from_raw(62),
                    mm: owner.mm,
                };
                assert!(!engine.native_stream_final_wait(sibling));
                assert_eq!(engine.terminal_stream_admission(owner, call).unwrap(), None);
                assert!(engine.terminal_stream_admission(sibling, call).is_err());
                let other = NetworkStreamOwner {
                    mm: owner.mm.for_exec(owner.thread),
                    ..owner
                };
                assert!(!engine.native_stream_final_wait(other));
                assert_eq!(engine.terminal_stream_admission(owner, call).unwrap(), None);
                assert!(engine.terminal_stream_admission(other, call).is_err());
                assert!(engine.native_stream_final_wait(owner));
            }
            let mut outside = runtime.controller_disposal_owner();
            // Controlled final-wait fixture above. This tests the real bounded
            // owner cleanup, not delivery of a kernel final-wait event.
            assert!(unsafe { outside.finish_native_controller_tasks().await }.is_err());
            runtime
                .shared
                .native_streams
                .lock()
                .unwrap()
                .settled()
                .unwrap();
            assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
            let state = engine.lock().unwrap();
            assert_eq!(state.native_capture_fixture_counts(ofd), (1, 1, 0, 0));
            let diagnostics = format!("{state:?}");
            assert!(diagnostics.contains("TerminalPinReleased"));
            assert!(diagnostics.contains("ReadPeekOffset"));
            assert!(diagnostics.contains(if raw_known {
                "PeekOffset(-1)"
            } else {
                "result: None"
            }));
            drop(state);
            let trace = Arc::try_unwrap(engine)
                .unwrap()
                .into_inner()
                .unwrap()
                .into_recorded_versioned_trace();
            assert!(
                matches!(trace, Err(NetworkReplayError::UnresolvedStreamCall(actual)) if actual == call)
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_terminal_reuses_known_close_after_lost_release_reply() {
        use std::io::Read;
        let (runtime, engine, owner, call, ofd, mut peer, _) = ordinary_call_fixture().await;
        engine
            .lock()
            .unwrap()
            .begin_stream_call_release(owner, call)
            .unwrap();
        runtime.release_native_stream(owner, call).await.unwrap();
        // The Guest callback never confirms its known close to the engine.
        assert!(engine.lock().unwrap().native_stream_final_wait(owner));
        let original = std::time::Instant::now();
        *runtime.shared.transport_terminal_deadline.lock().unwrap() = Some(original);
        let mut outside = runtime.controller_disposal_owner();
        assert!(unsafe { outside.finish_native_controller_tasks().await }.is_err());
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .settled()
            .unwrap();
        assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
        assert_eq!(
            engine.lock().unwrap().native_capture_fixture_counts(ofd),
            (1, 0, 0, 0)
        );
        let trace = Arc::try_unwrap(engine)
            .unwrap()
            .into_inner()
            .unwrap()
            .into_recorded_versioned_trace();
        assert!(
            matches!(trace, Err(crate::network_replay::NetworkReplayError::UnresolvedStreamCall(actual)) if actual == call)
        );
        assert!(
            runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .is_empty()
        );
        assert_eq!(
            *runtime.shared.transport_terminal_deadline.lock().unwrap(),
            Some(original)
        );
        assert!(unsafe { outside.finish_native_controller_tasks().await }.is_err());
        assert!(
            runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .is_empty()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_terminal_cancellation_waits_for_worker_and_keeps_original_deadline() {
        use std::io::Read;
        let (runtime, engine, owner, call, _, mut peer, fd) = ordinary_call_fixture().await;
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let mut effect = Box::pin(runtime.run_native_worker(move || {
            entered.send(()).unwrap();
            resume
                .recv_timeout(std::time::Duration::from_secs(1))
                .map_err(std::io::Error::other)
        }));
        assert!(futures::poll!(effect.as_mut()).is_pending());
        tokio::time::timeout(std::time::Duration::from_secs(1), ready)
            .await
            .unwrap()
            .unwrap();
        drop(effect);
        assert!(engine.lock().unwrap().native_stream_final_wait(owner));
        let mut outside = runtime.controller_disposal_owner();
        let mut finish = Box::pin(unsafe { outside.finish_native_controller_tasks() });
        assert!(futures::poll!(finish.as_mut()).is_pending());
        let original = runtime
            .shared
            .transport_terminal_deadline
            .lock()
            .unwrap()
            .unwrap();
        assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        assert!(
            runtime
                .shared
                .native_streams
                .lock()
                .unwrap()
                .settled()
                .is_err()
        );
        drop(finish);
        // Ordinary admission remains closed; no new capture/effect can enter.
        assert!(runtime.run_native_worker(|| Ok(())).await.is_err());
        release.send(()).unwrap();
        assert!(unsafe { outside.finish_native_controller_tasks().await }.is_err());
        assert_eq!(
            *runtime.shared.transport_terminal_deadline.lock().unwrap(),
            Some(original)
        );
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .settled()
            .unwrap();
        assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
        let trace = Arc::try_unwrap(engine)
            .unwrap()
            .into_inner()
            .unwrap()
            .into_recorded_versioned_trace();
        assert!(
            matches!(trace, Err(crate::network_replay::NetworkReplayError::UnresolvedStreamCall(actual)) if actual == call)
        );
        assert!(
            runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .is_empty()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_capture_canceled_waiter_exit_before_success_retires_exact_custody() {
        abandoned_capture_control(true, false, false).await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn native_capture_canceled_waiter_exit_before_failure_releases_shared_table() {
        abandoned_capture_control(false, false, false).await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn native_capture_canceled_waiter_success_before_exit_closes_original_pin() {
        abandoned_capture_control(true, true, false).await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn native_capture_canceled_waiter_failure_before_exit_releases_shared_table() {
        abandoned_capture_control(false, true, false).await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn native_capture_confirmed_pin_exit_before_control_finish_retires_once() {
        abandoned_capture_control(true, true, true).await;
    }

    #[tokio::test]
    async fn native_worker_success_requires_actual_join() {
        let (runtime, _) = fixture(71);
        assert_eq!(runtime.run_native_worker(|| Ok(42)).await.unwrap(), 42);
        assert!(
            runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .is_empty()
        );
        runtime
            .shared
            .finish_native_workers(std::time::Instant::now() + std::time::Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn native_worker_cancellation_keeps_run_owner_join_authority() {
        let (runtime, _) = fixture(72);
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, proceed) = std::sync::mpsc::channel();
        let mut waiter = Box::pin(runtime.run_native_worker(move || {
            entered.send(()).unwrap();
            proceed
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            Ok(())
        }));
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        tokio::time::timeout(std::time::Duration::from_secs(1), started)
            .await
            .unwrap()
            .unwrap();
        drop(waiter);
        assert_eq!(runtime.shared.native_workers.lock().unwrap().tasks.len(), 1);
        release.send(()).unwrap();
        runtime
            .shared
            .finish_native_workers(std::time::Instant::now() + std::time::Duration::from_secs(1))
            .await
            .unwrap();
        assert!(
            runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .is_empty()
        );
    }

    #[tokio::test]
    async fn native_poll_deadline_closes_admission_and_retains_late_worker() {
        let (runtime, _) = fixture(75);
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, proceed) = std::sync::mpsc::channel();
        let mut pending = Box::pin(runtime.run_native_worker(move || {
            let _ = entered.send(());
            proceed.recv_timeout(std::time::Duration::from_secs(1))
                .map_err(std::io::Error::other)?;
            Ok(())
        }));
        let was_pending = futures::poll!(pending.as_mut()).is_pending();
        let entered = tokio::time::timeout(std::time::Duration::from_secs(1), started).await;
        let worker = runtime.shared.native_workers.lock().unwrap().tasks[0].clone();
        drop(pending);
        let refusal = runtime.refuse_native_poll_deadline();
        let next = runtime.start_native_worker(|| Ok(()));
        let released = release.send(());
        let joined = runtime.shared.join_native_worker(&worker).await;
        // Semantic assertions follow the actual join, including if an earlier
        // premise failed. Late physical completion cannot turn the cap green.
        assert!(was_pending);
        assert!(matches!(entered, Ok(Ok(()))));
        assert!(released.is_ok());
        assert!(refusal.to_string().contains("original one-second helper deadline"));
        assert!(next.is_err());
        assert!(joined.is_err());
        let completion = worker.completion.lock().await;
        assert!(completion.task.is_none());
        assert_eq!(completion.terminal, Some(Ok(())));
        assert!(worker.deadline_failure.lock().unwrap().is_some());
        assert_eq!(runtime.shared.native_workers.lock().unwrap().tasks.len(), 1);
        assert!(runtime.shared.native_terminal_failure.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn native_worker_deadline_retains_task_and_never_becomes_late_success() {
        let (runtime, _) = fixture(73);
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, proceed) = std::sync::mpsc::channel();
        let mut waiter = Box::pin(runtime.run_native_worker(move || {
            entered.send(()).unwrap();
            proceed
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            Ok(())
        }));
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        tokio::time::timeout(std::time::Duration::from_secs(1), started)
            .await
            .unwrap()
            .unwrap();
        drop(waiter);
        let original = std::time::Instant::now() + std::time::Duration::from_millis(10);
        assert!(
            runtime
                .shared
                .finish_native_workers(original)
                .await
                .is_err()
        );
        let worker = runtime.shared.native_workers.lock().unwrap().tasks[0].clone();
        assert!(worker.completion.lock().await.task.is_some());
        release.send(()).unwrap();
        // Join actual completion with the same retained task; the timeout is
        // still a failure after cleanup, even when the late worker has ended.
        assert!(runtime.shared.join_native_worker(&worker).await.is_err());
        let state = worker.completion.lock().await;
        assert!(state.task.is_none());
        assert_eq!(state.terminal, Some(Ok(())));
        assert!(worker.deadline_failure.lock().unwrap().is_some());
        drop(state);
        assert!(
            runtime
                .shared
                .finish_native_workers(original)
                .await
                .is_err()
        );
    }

    #[test]
    fn helper_worker_reports_failure_before_quarantine_and_keeps_original_deadline() {
        const CHILD: &str = "HERMIT_HELPER_QUARANTINE_COMPONENT_CHILD";
        if let Ok(case) = std::env::var(CHILD) {
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            executor.block_on(async {
                let (runtime, _) = fixture(79);
                // Explicit component premise: registration is possibly live.
                // No provider syscall is executed by this ownership control.
                struct PendingProbe(Arc<AtomicUsize>);
                impl Drop for PendingProbe {
                    fn drop(&mut self) {
                        self.0.fetch_add(1, Ordering::SeqCst);
                    }
                }
                let released = Arc::new(AtomicUsize::new(0));
                let pending = Arc::new(PendingProbe(released.clone()));
                let quarantine = Arc::new(NativeQuarantine::default());
                quarantine.retain(pending.clone()).unwrap();
                quarantine.mark_possible().unwrap();
                let (entered, observed) = tokio::sync::oneshot::channel();
                let panic_case = case == "panic";
                let false_success = case == "success";
                let (worker, reply) = runtime
                    .shared
                    .start_native_worker_with_quarantine(
                        tokio::runtime::Handle::current(),
                        None,
                        false,
                        Some(quarantine.clone()),
                        move || {
                            let _ = entered.send(());
                            if panic_case {
                                panic!("component helper registration unknown");
                            }
                            if false_success {
                                return Ok(());
                            }
                            Err::<(), _>(std::io::Error::other(
                                "component helper registration unknown",
                            ))
                        },
                    )
                    .unwrap();
                if case == "cancel" {
                    drop(reply);
                } else {
                    let error = tokio::time::timeout(std::time::Duration::from_secs(1), reply)
                        .await
                        .expect("failure must be delivered before the parked JoinHandle")
                        .unwrap()
                        .unwrap_err();
                    assert!(error.to_string().contains(if false_success {
                        "quarantined helper cannot return idle success"
                    } else {
                        "component helper registration unknown"
                    }));
                }
                tokio::time::timeout(std::time::Duration::from_secs(1), observed)
                    .await
                    .unwrap()
                    .unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                assert!(
                    runtime
                        .shared
                        .finish_native_workers(deadline)
                        .await
                        .is_err()
                );
                assert!(
                    runtime
                        .shared
                        .finish_native_workers(deadline)
                        .await
                        .is_err()
                );
                assert!(!runtime.shared.native_workers_terminated());
                assert!(quarantine.is_possible());
                // Model removal of the Call and every callback-side reference.
                // Only the same parked wrapper's quarantine cell remains.
                drop(pending);
                drop(quarantine);
                assert_eq!(
                    released.load(Ordering::SeqCst),
                    0,
                    "FnOnce return or Call removal cannot free uncertain Pending custody"
                );
                assert!(worker.deadline_failure.lock().unwrap().is_some());
                let state = worker.completion.lock().await;
                assert!(state.terminal.is_none());
                assert!(!state.task.as_ref().unwrap().is_finished());
                drop(state);
                assert!(
                    Arc::strong_count(&runtime.shared) > 1,
                    "same quarantined closure retains Pending/task/scratch custody"
                );
            });
            executor.shutdown_background();
            // A controller process terminal is the production release. This
            // isolated child prevents a deliberately parked component worker
            // leaking into the parent test process or making Runtime::drop wait.
            std::process::exit(23);
        }
        for case in ["error", "panic", "cancel", "success"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "network_runtime::tests::helper_worker_reports_failure_before_quarantine_and_keeps_original_deadline", "--nocapture"])
                .env(CHILD, case).stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("helper quarantine component child exceeded its bound: {case}");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            assert_eq!(
                status.code(),
                Some(23),
                "case {case} must execute all assertions and actual process exit"
            );
        }
    }

    #[tokio::test]
    async fn native_worker_deadline_with_live_join_waiter_is_bounded() {
        let (runtime, _) = fixture(74);
        let (release, proceed) = std::sync::mpsc::channel();
        let task = tokio::task::spawn_blocking(move || {
            proceed
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            Ok(())
        });
        let worker = std::sync::Arc::new(NativeWorker {
            runtime: std::sync::Arc::downgrade(&runtime.shared),
            completion: tokio::sync::Mutex::new(NativeWorkerCompletion {
                task: Some(task),
                terminal: None,
            }),
            deadline_failure: Mutex::default(),
        });
        runtime
            .shared
            .native_workers
            .lock()
            .unwrap()
            .tasks
            .push(worker.clone());
        let mut live_waiter = Box::pin(runtime.shared.join_native_worker(&worker));
        assert!(futures::poll!(live_waiter.as_mut()).is_pending());
        assert!(
            worker.completion.try_lock().is_err(),
            "live waiter owns the pending join lock"
        );
        let original = std::time::Instant::now() + std::time::Duration::from_millis(10);
        let error = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            runtime.shared.finish_native_workers(original),
        )
        .await
        .expect("cleanup must not reacquire a blocked join lock after its deadline")
        .unwrap_err();
        assert!(error.to_string().contains("original terminal deadline"));
        assert!(worker.deadline_failure.lock().unwrap().is_some());
        assert!(
            runtime
                .shared
                .native_terminal_failure
                .lock()
                .unwrap()
                .is_some()
        );
        release.send(()).unwrap();
        assert!(live_waiter.await.is_err());
        assert!(worker.completion.lock().await.task.is_none());
        assert!(
            runtime
                .shared
                .finish_native_workers(original)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_terminal_cutoff_rejects_late_callback_before_physical_work() {
        let (runtime, _) = fixture(75);
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, proceed) = std::sync::mpsc::channel();
        let mut admitted = Box::pin(runtime.run_native_worker(move || {
            entered.send(()).unwrap();
            proceed
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            Ok(17)
        }));
        assert!(futures::poll!(admitted.as_mut()).is_pending());
        tokio::time::timeout(std::time::Duration::from_secs(1), started)
            .await
            .unwrap()
            .unwrap();
        let original = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut cleanup = Box::pin(runtime.shared.finish_native_workers(original));
        assert!(futures::poll!(cleanup.as_mut()).is_pending());
        assert!(runtime.shared.native_workers.lock().unwrap().closed);
        let touched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let physical = touched.clone();
        let late = runtime
            .run_native_worker(move || {
                physical.store(true, std::sync::atomic::Ordering::Release);
                Ok(())
            })
            .await
            .unwrap_err();
        assert!(late.to_string().contains("admission closed"));
        assert!(!touched.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(runtime.shared.native_workers.lock().unwrap().tasks.len(), 1);
        release.send(()).unwrap();
        assert!(
            cleanup
                .await
                .unwrap_err()
                .to_string()
                .contains("admission closed")
        );
        assert_eq!(admitted.await.unwrap(), 17);
        assert!(
            runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .is_empty()
        );
        assert!(
            runtime
                .shared
                .native_terminal_failure
                .lock()
                .unwrap()
                .is_some()
        );
        assert!(
            runtime
                .shared
                .finish_native_workers(original)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_terminal_cutoff_keeps_rejected_release_pin_in_custody() {
        let (runtime, _) = fixture(76);
        let thread = crate::types::DetTid::from_raw(76);
        let who = crate::network_replay::NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        let call = crate::network_replay::NetworkStreamCallId::controlled_fixture(76);
        let (original, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = original.as_raw_fd();
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .capture(who, call, original.into())
            .unwrap();
        let original_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        runtime
            .shared
            .finish_native_workers(original_deadline)
            .await
            .unwrap();
        let error = runtime.release_native_stream(who, call).await.unwrap_err();
        assert!(error.to_string().contains("admission closed"));
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) },
            -1,
            "rejected release must not close the pin"
        );
        assert!(
            runtime
                .shared
                .native_streams
                .lock()
                .unwrap()
                .settled()
                .is_err()
        );
        // Explicit finite fixture cleanup observes the actual close, then drops
        // its custody row. The failed run still cannot become a late success.
        let work = runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .prepare_release(who, call)
            .unwrap();
        let released = work.perform();
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .retain_release(who, call, released)
            .unwrap();
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .finish_release(who, call)
            .unwrap();
        assert!(
            runtime
                .shared
                .finish_native_workers(original_deadline)
                .await
                .is_err()
        );
        drop(peer);
    }

    #[tokio::test]
    async fn terminal_join_keeps_primary_failure_and_still_joins_later_owned_worker() {
        let (runtime, _) = fixture(77);
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (first, first_reply) = runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), || {
                Err::<(), _>(std::io::Error::other("primary worker failure"))
            })
            .unwrap();
        let flag = observed.clone();
        let (second, second_reply) = runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), move || {
                flag.store(true, std::sync::atomic::Ordering::Release);
                Ok(())
            })
            .unwrap();
        drop((first_reply, second_reply));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let error = runtime
            .shared
            .finish_native_workers(deadline)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "primary worker failure");
        assert!(observed.load(std::sync::atomic::Ordering::Acquire));
        assert!(first.completion.lock().await.task.is_none());
        assert!(second.completion.lock().await.task.is_none());
        assert_eq!(second.completion.lock().await.terminal, Some(Ok(())));
        assert_eq!(runtime.shared.native_workers.lock().unwrap().tasks.len(), 1);
        assert!(runtime.shared.native_workers.lock().unwrap().closed);
    }
    #[tokio::test]
    async fn closed_admission_allows_only_exact_owned_retirement_and_observes_real_pin_close() {
        use chrono::TimeZone;

        use crate::network_replay::NetworkReplayEngine;
        use crate::network_replay::NetworkStreamCallId;
        use crate::network_replay::NetworkStreamOwner;
        use crate::network_replay::original_connect::Admission;
        use crate::network_replay::original_connect::Arguments;
        use crate::types::DetTid;
        use crate::types::MmId;
        let (runtime, _) = fixture(78);
        let thread = DetTid::from_raw(78);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let call = NetworkStreamCallId::controlled_fixture(78);
        let admission = Admission {
            call,
            arguments: Arguments {
                kind: crate::network_replay::original_connect::Kind::Connect,
                operation: crate::resources::ExternalOpId::new(thread, 1),
                files: detcore_model::fd::FilesId::initial(thread),
                binding: None,
                fd: 7,
                address: 0x2000,
                length: 16,
                original_count: 0,
            },
        };
        let engine = Arc::new(Mutex::new(NetworkReplayEngine::record(
            chrono::Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
        )));
        let publication =
            NativeCaptureRecovery::new(engine, Arc::new(tokio::sync::Notify::new()), |_| {});
        let (pin, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = pin.as_raw_fd();
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .capture_original(
                owner,
                admission,
                Some(pin.into()),
                tokio::runtime::Handle::current(),
                publication,
            )
            .unwrap();
        runtime.shared.native_workers.lock().unwrap().closed = true;
        let executed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        for wrong in [call, NetworkStreamCallId::controlled_fixture(79)] {
            let flag = executed.clone();
            assert!(
                runtime
                    .shared
                    .start_original_retirement_worker(
                        owner,
                        wrong,
                        tokio::runtime::Handle::current(),
                        move || {
                            flag.store(true, std::sync::atomic::Ordering::Release);
                            Ok(())
                        }
                    )
                    .is_err()
            );
        }
        assert!(!executed.load(std::sync::atomic::Ordering::Acquire));
        assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        {
            let mut calls = runtime.shared.native_streams.lock().unwrap();
            let original = calls.original(owner, call).unwrap();
            original.retired = true;
            original.close_queued = true;
        }
        let shared = runtime.shared.clone();
        let (worker, reply) = runtime
            .shared
            .start_original_retirement_worker(
                owner,
                call,
                tokio::runtime::Handle::current(),
                move || {
                    let work = shared
                        .native_streams
                        .lock()
                        .unwrap()
                        .prepare_release(owner, call)?;
                    let raw = work.perform();
                    shared
                        .native_streams
                        .lock()
                        .unwrap()
                        .retain_release(owner, call, raw)
                },
            )
            .unwrap();
        reply.await.unwrap().unwrap();
        runtime.shared.join_native_worker(&worker).await.unwrap();
        assert!(
            runtime
                .shared
                .native_streams
                .lock()
                .unwrap()
                .original_closed(owner, call)
                .unwrap()
        );
        use std::io::Read;
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        assert_eq!(
            peer.read(&mut [0u8; 1]).unwrap(),
            0,
            "the peer observes actual final pin close"
        );
        runtime
            .shared
            .native_streams
            .lock()
            .unwrap()
            .finish_release(owner, call)
            .unwrap();
        assert!(
            runtime
                .shared
                .native_streams
                .lock()
                .unwrap()
                .settled()
                .is_ok()
        );
        assert!(
            runtime
                .shared
                .start_native_worker(tokio::runtime::Handle::current(), || Ok(()))
                .is_err()
        );
        drop(peer);
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
