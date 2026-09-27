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
use std::time::Instant;

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

/// Initialize Tokio's process-global signal socket pair in the outside
/// namespace. Drop the complete scratch reactor before ARM and clone. Tokio's
/// fresh child reactor then duplicates the existing CLOEXEC signal receiver;
/// no runtime, signal listener or active parent reactor crosses the clone.
/// The ordinary CLI does not construct another parent reactor while its child
/// is live, so nobody outside can consume the child's signal wakeup bytes.
fn prepare_controller_runtime_before_clone() -> io::Result<()> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(io::Error::other(
            "network Container startup requires no current parent Tokio runtime",
        ));
    }
    // Tokio's lazy signal-pair allocation itself uses expect. Preserve the
    // prepared guard owner even if that dependency initialization panics.
    let runtime = std::panic::catch_unwind(crate::new_controller_runtime).map_err(|panic| {
        let detail = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic");
        io::Error::other(format!(
            "controller runtime initialization panicked: {detail}"
        ))
    })??;
    drop(runtime);
    Ok(())
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
/// service thread, live runtime/reactor/listener, Arc owner or libbpf session may
/// cross that clone. The inert process-global Tokio signal pair is initialized
/// and its scratch reactor dropped before ARM; its FDs are CLOEXEC. The caller
/// must keep the parent free of active Tokio reactors/listeners until child exit.
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
    unsafe {
        run_with_network_startup_observed(
            container, startup_timeout, accepted_launch, prepared_guard,
            &mut |_, _| Ok(()), run,
        )
    }
}

/// The same owned exchange with a parent-only observer before STARTUP_READY.
/// It borrows the actual service owner, including partial startup failure, while
/// the stopped controller cannot run or end the service's PIDFD lifetime.
/// The observer must retain any command ownership when returning an error.
/// # Safety
/// All fork-safety and single-reaper requirements above apply unchanged. The
/// observer may start parent commands only after clone, within the original
/// startup deadline. It must not release or replace the borrowed service owner.
pub unsafe fn run_with_network_startup_observed<T, U, F, O>(
    container: &mut Container,
    startup_timeout: Duration,
    accepted_launch: Option<AcceptedProviderLaunch>,
    prepared_guard: Option<PreparedGuard>,
    observe_parent: &mut O,
    run: &mut F,
) -> Result<NetworkContainerRun<T>, NetworkStartupFailure>
where
    T: serde::Serialize,
    F: FnMut(
        Option<NetworkRuntimeResources>,
        Option<Arc<dyn NetworkGuardControl>>,
        Option<&mut GuardControllerOwner>,
    ) -> (T, U),
    O: FnMut(&NetworkParentOwnership, Instant) -> Result<(), StartupError>,
{
    unsafe {
        run_with_network_startup_hooked(container, startup_timeout, accepted_launch,
            prepared_guard, &mut NoGroupedHook, observe_parent, run)
    }
}

struct NoGroupedHook;
impl detcore::network_runtime::AcceptedPostSpawn for NoGroupedHook {
    fn after_spawn(&mut self, _: detcore::network_runtime::AcceptedSpawned<'_>)
        -> io::Result<Option<detcore::network_runtime::GroupedBootstrapTransport>> {
        Ok(None)
    }
}

/// The same exchange with retained broker startup between the actual service
/// spawn and Bootstrap. The hook remains owned by the caller on every error.
/// # Safety
/// All fork, deadline and single-reaper requirements above apply unchanged.
pub unsafe fn run_with_network_startup_hooked<T, U, F, O>(
    container: &mut Container,
    startup_timeout: Duration,
    accepted_launch: Option<AcceptedProviderLaunch>,
    prepared_guard: Option<PreparedGuard>,
    accepted_hook: &mut dyn detcore::network_runtime::AcceptedPostSpawn,
    observe_parent: &mut O,
    run: &mut F,
) -> Result<NetworkContainerRun<T>, NetworkStartupFailure>
where
    T: serde::Serialize,
    F: FnMut(
        Option<NetworkRuntimeResources>,
        Option<Arc<dyn NetworkGuardControl>>,
        Option<&mut GuardControllerOwner>,
    ) -> (T, U),
    O: FnMut(&NetworkParentOwnership, Instant) -> Result<(), StartupError>,
{
    if prepared_guard.is_some() {
        if let Err(error) = prepare_controller_runtime_before_clone() {
            return Err(NetworkStartupFailure {
                cause: StartupError::Protocol,
                guard_error: Some(error),
                guard: prepared_guard.map(PreparedGuard::into_parent),
            });
        }
    }
    // This transport is allocated before ARM/clone, outside the protected guest
    // namespace. Guard-only mode creates no accepted transport or service.
    let wire_format = accepted_launch.as_ref().map(|launch| launch.expected.wire_format);
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
            let started = match unsafe {
                ParentAcceptedService::start_after_clone_with_hook(
                    service_launch,
                    endpoint,
                    context
                        .child_pidfd()
                        .try_clone_to_owned()
                        .map_err(|_| StartupError::Protocol)?,
                    incarnation.ok_or(StartupError::Protocol)?,
                    context.deadline(),
                    accepted_hook,
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
            };
            let observed = observe_parent(parent.as_ref().expect("service owner retained"), context.deadline());
            // Preserve the original service failure, with both owners retained.
            started.and(observed)
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
            let (owner, resource) = match (controller, incarnation, wire_format) {
                (Some(controller), Some(incarnation), Some(wire_format)) => {
                    let (owner, resource) = unsafe {
                        NetworkRuntimeResources::from_authenticated_startup(controller, incarnation, wire_format)
                    };
                    (Some(owner), Some(resource))
                }
                (None, None, None) => (None, None),
                _ => return Err(StartupError::Protocol),
            };
            // Only this post-clone branch constructs shared ownership, mutexes
            // or an eventual observer thread. Nothing is copied across clone.
            let (guard, guard_resource) = match guard {
                Some(guard) => {
                    let (owner, resource) = unsafe {
                        GuardControllerOwner::from_controller(
                            guard,
                            context
                                .deadline()
                                .min(armed.as_ref().expect("guard arm").deadline()),
                        )
                    };
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

#[cfg(test)]
mod controller_runtime_tests {
    use std::collections::BTreeMap;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::time::Instant;

    use super::*;

    const CONTROL: &str = "HERMIT_CONTROLLER_RUNTIME_CONTROL";
    const EXACT: &str = "network_container::controller_runtime_tests::preclone_signal_transport_survives_two_children_without_new_socketpairs";

    fn sockets() -> BTreeMap<i32, String> {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let fd = entry.file_name().to_str()?.parse().ok()?;
                let target = std::fs::read_link(entry.path()).ok()?;
                let target = target.to_str()?.to_owned();
                target.starts_with("socket:[").then_some((fd, target))
            })
            .collect()
    }

    fn deny_new_socketpair() {
        let mut filters = [
            libc::sock_filter {
                code: 0x20,
                jt: 0,
                jf: 0,
                k: 0,
            },
            libc::sock_filter {
                code: 0x15,
                jt: 0,
                jf: 1,
                k: libc::SYS_socketpair as u32,
            },
            libc::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: 0x00050000 | libc::EPERM as u32,
            },
            libc::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: 0x7fff0000,
            },
        ];
        let program = libc::sock_fprog {
            len: filters.len() as u16,
            filter: filters.as_mut_ptr(),
        };
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
            0
        );
        assert_eq!(
            unsafe { libc::syscall(libc::SYS_seccomp, 1, 0, &program) },
            0
        );
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
            -1
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
    }

    #[test]
    fn preclone_signal_transport_survives_two_children_without_new_socketpairs() {
        match std::env::var(CONTROL).as_deref() {
            Ok("exec") => {
                let forbidden =
                    std::env::var("HERMIT_CONTROLLER_RUNTIME_SOCKET_IDENTITIES").unwrap();
                let live = sockets();
                for identity in forbidden.lines() {
                    assert!(
                        !live.values().any(|actual| actual == identity),
                        "controller signal FD survived exec"
                    );
                }
                return;
            }
            Ok("denied") => {
                deny_new_socketpair();
                let error = prepare_controller_runtime_before_clone().unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("controller runtime initialization panicked")
                );
                return;
            }
            Ok("fork") => {}
            _ => {
                for mode in ["denied", "fork"] {
                    // Isolate Tokio's process-global state from all other tests.
                    let mut child = Command::new(std::env::current_exe().unwrap())
                        .args(["--exact", EXACT, "--test-threads=1", "--nocapture"])
                        .env(CONTROL, mode)
                        .process_group(0)
                        .spawn()
                        .unwrap();
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let mut status = None;
                    while Instant::now() < deadline {
                        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
                        assert_eq!(
                            unsafe {
                                libc::waitid(
                                    libc::P_PID,
                                    child.id(),
                                    &mut info,
                                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                                )
                            },
                            0
                        );
                        if unsafe { info.si_pid() } != 0 {
                            status = Some(());
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    // Pin the process-group identity with the unreaped child.
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    let actual = child.wait().unwrap();
                    assert!(
                        status.is_some() && actual.success(),
                        "isolated control: {actual}"
                    );
                    assert_eq!(unsafe { libc::kill(-(child.id() as i32), 0) }, -1);
                    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
                }
                return;
            }
        }
        let before = sockets();
        prepare_controller_runtime_before_clone().unwrap();
        let after = sockets();
        let pair: Vec<_> = after
            .iter()
            .filter(|(fd, _)| !before.contains_key(fd))
            .collect();
        assert_eq!(pair.len(), 2, "only the inert global signal pair remains");
        for (fd, _) in &pair {
            assert_ne!(
                unsafe { libc::fcntl(**fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        let forbidden = pair
            .iter()
            .map(|(_, identity)| identity.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let runtime = crate::new_controller_runtime().unwrap();
        let entered = runtime.enter();
        assert!(prepare_controller_runtime_before_clone().is_err());
        drop(entered);
        drop(runtime);
        for _ in 0..2 {
            prepare_controller_runtime_before_clone().unwrap();
            assert_eq!(sockets(), after, "scratch runtime leaves no reactor FD");
            let child = Container::new()
                .run_with_startup_owned(
                    Duration::from_secs(2),
                    &mut |_| Ok(()),
                    &mut |_| Ok(()),
                    &mut |_| {
                        deny_new_socketpair();
                        let runtime = crate::new_controller_runtime().unwrap();
                        runtime.block_on(async {
                            let mut signal = tokio::signal::unix::signal(
                                tokio::signal::unix::SignalKind::user_defined1(),
                            )
                            .unwrap();
                            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
                            assert!(
                                tokio::time::timeout(Duration::from_secs(1), signal.recv())
                                    .await
                                    .unwrap()
                                    .is_some()
                            );
                            assert!(
                                tokio::process::Command::new(std::env::current_exe().unwrap())
                                    .args(["--exact", EXACT, "--test-threads=1"])
                                    .env(CONTROL, "exec")
                                    .env("HERMIT_CONTROLLER_RUNTIME_SOCKET_IDENTITIES", &forbidden)
                                    .status()
                                    .await
                                    .unwrap()
                                    .success()
                            );
                        });
                        drop(runtime);
                        (17u32, ())
                    },
                )
                .unwrap();
            let actual_pid = child.cleanup().child_pid();
            let reverie::process::OwnedFinalize::Complete(done) =
                child.finalize_until(Instant::now() + Duration::from_secs(2))
            else {
                panic!("actual Container child not settled");
            };
            assert_eq!(done.decode().unwrap(), 17);
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(actual_pid.as_raw(), &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
        assert_eq!(sockets(), after);
    }
}
