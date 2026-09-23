//! Owned network-service startup outside the guest PID namespace.
//!
//! This accepted-only entry remains gated on the separately qualified provider
//! launch route; it does not activate the unrelated full FD mutation service. It uses the real owned Container startup mechanism;
//! no copied parent Arc/thread or serialized raw FD grants child authority.
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::time::Duration;

use detcore::network_runtime::AcceptedProviderLaunch;
use detcore::network_runtime::NetworkRuntimeOwner;
use detcore::network_runtime::NetworkRuntimeResources;
use detcore::network_runtime::ParentAcceptedService;
use detcore::network_runtime::ParentAcceptedStartFailure;
use reverie::process::Container;
use reverie::process::OwnedDeferredContainerRun;
use reverie::process::StartupError;
use reverie::process::StartupOwnedFailure;

/// Parent-owned external recovery, kept beside every container outcome.
#[must_use]
pub enum NetworkParentOwnership {
    /// Actual service/endpoint/controller pidfd, established before STARTUP_READY.
    Running(ParentAcceptedService),
    /// Actual bootstrap failure retains any broker cleanup capability.
    StartupFailed(ParentAcceptedStartFailure),
}

/// The container status and external owner are deliberately not collapsed into
/// anyhow/string errors. Finalize the exact child, drain the matching service,
/// then decode the container bytes through the original result classifier.
#[must_use = "finalize the actual child and outside service before decoding or discarding"]
pub struct NetworkContainerRun<T> {
    /// Original ownership-bearing startup/work result, with encoded bytes.
    pub container: Result<OwnedDeferredContainerRun<T>, StartupOwnedFailure<T>>,
    /// Outside service or bootstrap recovery; None means no service was started.
    pub parent: Option<NetworkParentOwnership>,
}

struct ChildNetworkStartup {
    resource: Option<NetworkRuntimeResources>,
    owner: NetworkRuntimeOwner,
}

/// Run using an actual startup-owned container and a same-run private channel.
///
/// # Safety
/// The original Container fork-safety contract applies: invoke before threads,
/// without a competing child reaper. `run` must use the supplied runtime only
/// for this contained ptrace/E9 controller and retain the existing panic/guard
/// wrapper and original work deadline. This helper does not invent a new guest
/// timeout. The ordinary backend lifetime capability remains absent.
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
    // Private authentication identity, generated outside the guest. It never
    // contributes to stable channel matching or guest scheduling.
    let incarnation = *uuid::Uuid::new_v4().as_bytes();
    let mut parent = None;
    let mut launch = Some(launch);
    let result = container.run_with_startup_owned(
        startup_timeout,
        &mut |mut context| {
            if context.descriptor_count() != 1 {
                return Err(StartupError::Protocol);
            }
            let endpoint = context.take_fd(0).ok_or(StartupError::Protocol)?;
            // Parent callback is after actual clone, before child workload
            // permission; service threads/broker therefore stay outside the
            // child's PID namespace and were not copied into it.
            match unsafe {
                ParentAcceptedService::start_after_clone(
                    launch.take().ok_or(StartupError::Protocol)?,
                    endpoint,
                    context
                        .child_pidfd()
                        .try_clone_to_owned()
                        .map_err(|_| StartupError::Protocol)?,
                    incarnation,
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
            // The pair is created only after namespace setup. Startup's SCM
            // transfer closes the child's parent endpoint before workload.
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
                return Err(StartupError::Io(reverie::Errno::last()));
            }
            let controller = unsafe { OwnedFd::from_raw_fd(raw[0]) };
            let outside = unsafe { OwnedFd::from_raw_fd(raw[1]) };
            context.transfer_fd(outside)?;
            let (owner, resource) = unsafe {
                NetworkRuntimeResources::from_authenticated_startup(controller, incarnation)
            };
            Ok(ChildNetworkStartup {
                resource: Some(resource),
                owner,
            })
        },
        &mut |mut startup: ChildNetworkStartup| {
            let resource = startup
                .resource
                .take()
                .expect("one startup resource for one controller");
            let (value, deferred) = run(resource);
            // Owner remains outside TracerBuilder/GlobalState even if its
            // future failed or was cancelled. U retains existing deferred data.
            (value, (deferred, startup.owner))
        },
    );
    NetworkContainerRun {
        container: result,
        parent,
    }
}
