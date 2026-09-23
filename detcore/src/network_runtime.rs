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
pub(crate) mod accepted_listener;
mod accepted_parent;
mod accepted_provider;
mod accepted_provider_ffi;
mod accepted_service;
mod accepted_transport;
pub mod capability_unit;
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
    endpoint: Option<OwnedFd>,
    controller: Mutex<Option<Result<std::sync::Arc<accepted_controller::Controller>, String>>>,
    incarnation: [u8; 16],
    physical: Mutex<physical::CustodyTasks<OwnedFd>>,
    accepted: Mutex<accepted::AcceptedCustody<OwnedFd>>,
    listeners: Mutex<accepted_listener::Listeners<OwnedFd>>,
    creations: tokio::sync::Mutex<accepted_creation::Creations>,
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
                endpoint: None,
                controller: Mutex::default(),
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
            endpoint: Some(endpoint),
            controller: Mutex::default(),
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
            *retained = Some(outcome);
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
        accepts.capture(
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
        )
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
                    endpoint: None,
                    controller: Mutex::default(),
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
