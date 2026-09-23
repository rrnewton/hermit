//! Owned network-service startup outside the guest PID namespace.
//!
//! Optional accepted-service and Unix-policy owners use one actual Container
//! exchange. Default Deny needs only the guard; no accepted socket materializer
//! or generic FD mutation capability is implied by this startup composition.
use std::cell::Cell;
use std::io;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Duration;

use detcore::network_runtime::AcceptedProviderLaunch;
use detcore::network_runtime::NetworkRuntimeOwner;
use detcore::network_runtime::NetworkRuntimeResources;
use detcore::network_runtime::ParentAcceptedService;
use detcore::network_runtime::ParentAcceptedStartFailure;
use detcore::network_runtime::guard::NetworkGuardControl;
use reverie::process::Container;
use reverie::process::OwnedDeferredContainerRun;
use reverie::process::StartupError;
use reverie::process::StartupOwnedFailure;

use crate::unix_guard::ParentGuard;
use crate::unix_guard::PreparedGuard;
use crate::unix_guard_control::GuardControllerOwner;

/// Parent-owned external recovery, kept beside every container outcome.
#[must_use]
pub enum NetworkParentOwnership {
    /// Actual service/endpoint/controller pidfd, established before STARTUP_READY.
    Running(ParentAcceptedService),
    /// Actual bootstrap failure retains any broker cleanup capability.
    StartupFailed(ParentAcceptedStartFailure),
}

/// Finalize the exact child and both matching outside owners before decoding
/// container bytes. No field is a substitute for the other owners' proof.
#[must_use = "finalize the actual child and outside owners before decoding or discarding"]
pub struct NetworkContainerRun<T> {
    pub container: Result<OwnedDeferredContainerRun<T>, StartupOwnedFailure<T>>,
    /// None means the accepted service was never started, including guard-only.
    pub parent: Option<NetworkParentOwnership>,
    /// Includes failed startup and unresolved creator-mask recovery obligations.
    pub guard: Option<ParentGuard>,
}

/// An error before Container clone still owns the prepared/armed policy.
/// Keep the original error; later terminal failures are secondary context.
#[must_use = "recover the guard owner before ordinary continuation"]
pub struct NetworkStartupFailure {
    pub cause: StartupError,
    /// Detailed ARM/deadline error; the guard's typed outcome is read separately.
    pub guard_error: Option<io::Error>,
    pub guard: Option<ParentGuard>,
}

struct ChildNetworkStartup {
    resource: Option<NetworkRuntimeResources>,
    owner: Option<NetworkRuntimeOwner>,
    guard: Option<GuardControllerOwner>,
    guard_resource: Option<Arc<dyn NetworkGuardControl>>,
}

/// Run with independently optional accepted service and prepared Unix guard.
///
/// `run` receives an owned guard resource plus a recovery-owner borrow. The
/// owner is retained outside the backend future; the resource may enter actual
/// GlobalState initialization. Enroll the authentic stopped initial task before
/// resume, start its independent observer, then latch a real backend failure
/// before observer stop. Success may be latched only after stop/join and final
/// snapshot. This wrapper does not manufacture those backend/terminal proofs.
///
/// # Safety
/// Apply Container's fork-safety/no-competing-reaper contract. `prepared_guard`
/// must belong to this creator thread and exact impending CLONE_NEWNET. No
/// service thread, Arc, Tokio object or libbpf session may cross that clone.
/// Retain the existing panic/guard wrapper and work deadline inside `run`.
/// This function neither changes a guest deadline nor grants a new capability.
pub unsafe fn run_with_network_startup<T, U, F>(
    container: &mut Container,
    startup_timeout: Duration,
    accepted_launch: Option<AcceptedProviderLaunch>,
    prepared_guard: Option<PreparedGuard>,
    run: &mut F,
) -> Result<NetworkContainerRun<T>, NetworkStartupFailure>
where
    T: serde::Serialize,
    F: FnMut(
        Option<NetworkRuntimeResources>,
        Option<Arc<dyn NetworkGuardControl>>,
        Option<&mut GuardControllerOwner>,
    ) -> (T, U),
{
    // This transport is allocated before ARM/clone, outside the protected guest
    // namespace. Guard-only mode creates no accepted transport or service.
    let mut incarnation = None;
    let mut endpoints = None;
    if accepted_launch.is_some() {
        incarnation = Some(*uuid::Uuid::new_v4().as_bytes());
        let mut raw = [-1; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                raw.as_mut_ptr(),
            )
        } < 0
        {
            let cause = StartupError::Io(reverie::Errno::last());
            return Err(NetworkStartupFailure {
                cause,
                guard_error: None,
                guard: prepared_guard.map(PreparedGuard::into_parent),
            });
        }
        endpoints = Some((unsafe { OwnedFd::from_raw_fd(raw[0]) }, unsafe {
            OwnedFd::from_raw_fd(raw[1])
        }));
    }
    let (controller, outside) = match endpoints {
        Some((controller, outside)) => (Some(controller), Some(outside)),
        None => (None, None),
    };
    let controller_endpoint = Cell::new(controller);
    let outside_endpoint = Cell::new(outside);
    let armed = match prepared_guard {
        Some(prepared) => match unsafe { prepared.arm(startup_timeout) } {
            Ok(armed) => Some(armed),
            Err(failure) => {
                drop(controller_endpoint.take());
                drop(outside_endpoint.take());
                return Err(NetworkStartupFailure {
                    cause: StartupError::Protocol,
                    guard_error: Some(failure.error),
                    guard: Some(failure.owner),
                });
            }
        },
        None => None,
    };
    // ARM consumes the original startup budget; it does not create another one.
    let remaining = match armed.as_ref().map(|guard| guard.remaining()).transpose() {
        Ok(remaining) => remaining.unwrap_or(startup_timeout),
        Err(error) => {
            drop(controller_endpoint.take());
            drop(outside_endpoint.take());
            return Err(NetworkStartupFailure {
                cause: StartupError::InvalidTimeout,
                guard_error: Some(error),
                guard: armed.map(|guard| guard.into_parent()),
            });
        }
    };
    let mut parent = None;
    let mut launch = accepted_launch;
    let result = container.run_with_startup_owned(
        remaining,
        &mut |context| {
            // Each branch closes only its own COW alias, never a saved number.
            drop(controller_endpoint.take());
            if context.descriptor_count() != 0 {
                return Err(StartupError::Protocol);
            }
            if let Some(guard) = armed.as_ref() {
                guard.parent_startup(context.child_pidfd(), 0, context.deadline())?;
            }
            let Some(service_launch) = launch.take() else {
                return Ok(());
            };
            let endpoint = outside_endpoint.take().ok_or(StartupError::Protocol)?;
            // Parent starts this service only after actual clone. Its threads
            // and broker remain outside the guest PID namespace.
            match unsafe {
                ParentAcceptedService::start_after_clone(
                    service_launch,
                    endpoint,
                    context
                        .child_pidfd()
                        .try_clone_to_owned()
                        .map_err(|_| StartupError::Protocol)?,
                    incarnation.ok_or(StartupError::Protocol)?,
                    context.deadline(),
                )
            } {
                Ok(service) => {
                    parent = Some(NetworkParentOwnership::Running(service));
                    Ok(())
                }
                Err(failure) => {
                    parent = Some(NetworkParentOwnership::StartupFailed(failure));
                    Err(StartupError::Protocol)
                }
            }
        },
        &mut |context| {
            drop(outside_endpoint.take());
            let controller = controller_endpoint.take();
            // This explicitly closes outside-only guard FDs and restores the
            // child's inherited mask before constructing any backend/runtime.
            let guard = match armed.as_ref() {
                Some(guard) => Some(unsafe { guard.child_startup() }?),
                None => None,
            };
            let (owner, resource) = match (controller, incarnation) {
                (Some(controller), Some(incarnation)) => {
                    let (owner, resource) = unsafe {
                        NetworkRuntimeResources::from_authenticated_startup(controller, incarnation)
                    };
                    (Some(owner), Some(resource))
                }
                (None, None) => (None, None),
                _ => return Err(StartupError::Protocol),
            };
            // Only this post-clone branch constructs shared ownership, mutexes
            // or an eventual observer thread. Nothing is copied across clone.
            let (guard, guard_resource) = match guard {
                Some(guard) => {
                    let (owner, resource) =
                        unsafe { GuardControllerOwner::from_controller(guard, context.deadline()) };
                    (Some(owner), Some(resource))
                }
                None => (None, None),
            };
            Ok(ChildNetworkStartup {
                resource,
                owner,
                guard,
                guard_resource,
            })
        },
        &mut |mut startup: ChildNetworkStartup| {
            let (value, deferred) = run(
                startup.resource.take(),
                startup.guard_resource.take(),
                startup.guard.as_mut(),
            );
            // Keep actual child capabilities outside cancelled tracer futures,
            // and until after the original deferred-result publication.
            (value, (deferred, startup.owner, startup.guard))
        },
    );
    // Covers clone failure, child-startup failure and parent-callback failure.
    // The returned Container result still owns any actual child/reap obligation.
    drop(controller_endpoint.take());
    drop(outside_endpoint.take());
    Ok(NetworkContainerRun {
        container: result,
        parent,
        guard: armed.map(|guard| guard.into_parent()),
    })
}

/// Compatibility entry for callers that require only the accepted service.
/// # Safety
/// The same contract as `run_with_network_startup` applies.
pub unsafe fn run_with_accepted_network_startup<T, U, F>(
    container: &mut Container,
    startup_timeout: Duration,
    launch: AcceptedProviderLaunch,
    run: &mut F,
) -> NetworkContainerRun<T>
where
    T: serde::Serialize,
    F: FnMut(NetworkRuntimeResources) -> (T, U),
{
    match unsafe {
        run_with_network_startup(
            container,
            startup_timeout,
            Some(launch),
            None,
            &mut |resource, guard_resource, guard_owner| {
                assert!(guard_resource.is_none() && guard_owner.is_none());
                run(resource.expect("accepted launch supplies its runtime"))
            },
        )
    } {
        Ok(result) => result,
        Err(failure) => {
            // With no supplied guard, only the original socketpair error can
            // reach this branch. Do not silently discard any recovery owner.
            assert!(failure.guard.is_none() && failure.guard_error.is_none());
            NetworkContainerRun {
                container: Err(StartupOwnedFailure::BeforeClone {
                    cause: failure.cause,
                }),
                parent: None,
                guard: None,
            }
        }
    }
}
