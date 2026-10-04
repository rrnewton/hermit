/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Ordinary OSS KVM CLI ownership. No broker is started by a library callback.
//! The original main thread owns and reaps it; each real container adopts a
//! dedicated channel. Nothing here changes guest scheduling or host signals.

use std::mem::ManuallyDrop;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::panic::AssertUnwindSafe;

use hermit::Error;
use reverie_kvm::native_exit_broker::BrokerClient;
use reverie_kvm::native_exit_broker::BrokerOwner;
use reverie_kvm::native_exit_broker::ExecClientFailure;
use reverie_kvm::native_exit_broker::StartupAuthority;

/// Borrowed diagnostics while the original work result/panic and owner remain
/// retained. These PIDs are observations, never authority to signal descendants.
pub(super) struct FatalInvocation<'a> {
    pub broker_pid: libc::pid_t,
    pub cleanup: &'a Error,
    pub primary: Option<&'a Error>,
    pub panic: Option<&'a (dyn std::any::Any + Send)>,
}

#[cfg(test)]
fn with_early_owner<T>(
    enabled: bool,
    work: impl FnOnce(Option<&BrokerOwner>) -> Result<T, Error>,
) -> Result<T, Error> {
    with_early_owner_reporting(enabled, work, |_| {}, || {})
}

/// Called only by the real synchronous main, after parsing and before
/// evidence/log/run setup. Libtest and library callers cannot assert the
/// early-main contract by entering a backend helper.
/// The reporting callback cannot turn an abort into success.
/// `normally_settled` runs only after actual zero-status broker wait or
/// an unconfigured invocation with no broker, and never on a fatal path.
pub(super) fn with_early_owner_reporting<T>(
    enabled: bool,
    work: impl FnOnce(Option<&BrokerOwner>) -> Result<T, Error>,
    mut report_fatal: impl FnMut(FatalInvocation<'_>),
    normally_settled: impl FnOnce(),
) -> Result<T, Error> {
    if !enabled || !cfg!(hermit_oss_early_main) {
        // Unproved startup builds keep ordinary, unconfigured KVM working.
        // They do not claim parent-death support: producer nonzero SET refuses
        // without terminal cleanup. Only an attempted setup failure is fatal.
        let result = work(None);
        normally_settled();
        return result;
    }
    let work_ran = std::cell::Cell::new(false);
    let attached = std::cell::Cell::new(false);
    let reaped_normally = std::cell::Cell::new(false);
    let result = with_optional_peer_pidfd_capability(
        probe_peer_pidfd,
        |owner| {
            work_ran.set(true);
            work(owner)
        },
        |work| {
            attached.set(true);
            // SAFETY: the Cargo/OSS main is synchronous and its pinned fbinit wrapper
            // starts no threads. This call precedes tracing, Tokio and guest setup.
            // No owner is captured by a child callback. On normal completion this
            // main thread stays alive, without exec or credential/namespace changes,
            // through the exact wait. The only fatal exit exception is the explicit
            // abort_failed_invocation operation below, with no successful receipt.
            let authority = unsafe { StartupAuthority::assert_exclusive_early_launch() };
            let owner = match BrokerOwner::bootstrap(authority) {
                Ok(owner) => owner,
                Err(failure) => {
                    let primary = anyhow::anyhow!("native exit broker bootstrap: {failure:?}");
                    let mut retained = ManuallyDrop::new(failure);
                    if let Some(owner) = retained.child.take() {
                        let mut owner = ManuallyDrop::new(*owner);
                        let settlement =
                            std::panic::catch_unwind(AssertUnwindSafe(|| settle(&mut owner)));
                        let settlement = match settlement {
                            Ok(settlement) => settlement,
                            Err(payload) => {
                                let _retained_panic = ManuallyDrop::new(payload);
                                abort_invocation(
                                    owner,
                                    Ok(Err::<T, _>(primary)),
                                    anyhow::anyhow!("bootstrap child settlement panicked"),
                                    &mut report_fatal,
                                );
                            }
                        };
                        match settlement {
                            Settlement::Reaped(Ok(())) => {
                                reaped_normally.set(true);
                                // SAFETY: actual wait consumed this exact native child.
                                drop(unsafe { ManuallyDrop::take(&mut owner) });
                            }
                            Settlement::Reaped(Err(cleanup)) => {
                                drop(unsafe { ManuallyDrop::take(&mut owner) });
                                return Err(primary
                                    .context(format!("bootstrap child cleanup: {cleanup:#}")));
                            }
                            Settlement::Unconfirmed(cleanup) => abort_invocation(
                                owner,
                                Ok(Err::<T, _>(primary)),
                                cleanup,
                                &mut report_fatal,
                            ),
                        }
                    }
                    return Err(primary);
                }
            };
            // Retain owner even if work, settlement, or diagnostic reporting panics.
            let mut owner = ManuallyDrop::new(owner);
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| work(Some(&owner))));
            if super::owned_container::has_retained_owner() {
                abort_invocation(
                    owner,
                    result,
                    anyhow::anyhow!("container producer remains unresolved"),
                    &mut report_fatal,
                );
            }
            let settlement = std::panic::catch_unwind(AssertUnwindSafe(|| settle(&mut owner)));
            let settled = match settlement {
                Ok(Settlement::Reaped(settled)) => settled,
                Ok(Settlement::Unconfirmed(cleanup)) => {
                    abort_invocation(owner, result, cleanup, &mut report_fatal)
                }
                Err(payload) => {
                    // Preserve both the work result and this secondary panic. The
                    // existing panic hook has observed the original panic payload.
                    let _retained_panic = ManuallyDrop::new(payload);
                    abort_invocation(
                        owner,
                        result,
                        anyhow::anyhow!("native broker settlement panicked"),
                        &mut report_fatal,
                    );
                }
            };
            if settled.is_ok() {
                reaped_normally.set(true);
            }
            // SAFETY: every Reaped branch is backed by the exact existing try_wait.
            drop(unsafe { ManuallyDrop::take(&mut owner) });
            match result {
                Ok(Ok(value)) => settled.map(|()| value),
                Ok(Err(primary)) => match settled {
                    Ok(()) => Err(primary),
                    Err(cleanup) => {
                        Err(primary.context(format!("native broker cleanup failed: {cleanup:#}")))
                    }
                },
                Err(payload) => {
                    if let Err(cleanup) = settled {
                        eprintln!("HERMIT_CLEANUP_FAILED: native broker after panic: {cleanup:#}");
                    }
                    std::panic::resume_unwind(payload)
                }
            }
        },
    );
    if reaped_normally.get() || (!attached.get() && work_ran.get()) {
        normally_settled();
    }
    result
}

fn abort_invocation<T>(
    mut owner: ManuallyDrop<BrokerOwner>,
    result: std::thread::Result<Result<T, Error>>,
    cleanup: Error,
    report: &mut impl FnMut(FatalInvocation<'_>),
) -> ! {
    let result = ManuallyDrop::new(result);
    let cleanup = ManuallyDrop::new(cleanup);
    let primary = match &*result {
        Ok(Err(error)) => Some(error),
        _ => None,
    };
    let panic = match &*result {
        Err(payload) => Some(payload.as_ref()),
        _ => None,
    };
    let diagnostic = FatalInvocation {
        broker_pid: owner.native_pid(),
        cleanup: &cleanup,
        primary,
        panic,
    };
    let report_result = std::panic::catch_unwind(AssertUnwindSafe(|| report(diagnostic)));
    // A failed reporter cannot unwind into ordinary cleanup or fabricate a
    // successful run. Its original panic remains retained through native exit.
    let _retained_report = ManuallyDrop::new(report_result);
    // SAFETY: this real CLI selected fatal invocation failure. Original work,
    // primary/panic, pending owners and diagnostics remain retained. This is
    // neither SHUTDOWN nor a successful wait/client-containment claim.
    match unsafe { ManuallyDrop::take(&mut owner).abort_failed_invocation() } {
        Ok(never) => match never {},
        Err((cause, original_owner)) => {
            // Exact creator mismatch is not bypassed by another exit API.
            // This contract violation retains the same owner for supervision.
            let _retained_cause = ManuallyDrop::new(cause);
            retain_original_thread(original_owner, "fatal abort refused exact creator identity");
        }
    }
}

/// Branch only on host capability availability. This private seam carries no
/// startup authority: the real early-main caller alone constructs that proof.
/// The probe has no guest-visible effects and grants no process-death authority.
fn with_optional_peer_pidfd_capability<T, W>(
    probe: impl FnOnce() -> std::io::Result<bool>,
    work: W,
    supported: impl FnOnce(W) -> Result<T, Error>,
) -> Result<T, Error>
where
    W: FnOnce(Option<&BrokerOwner>) -> Result<T, Error>,
{
    let available = probe().map_err(|error| {
        anyhow::Error::new(error).context("native exit SO_PEERPIDFD capability probe")
    })?;
    if available {
        // A later bootstrap/adoption error must never retry work unconfigured.
        supported(work)
    } else {
        // Ordinary KVM remains available without the optional cleanup protocol.
        // The unconfigured producer still refuses nonzero PDEATHSIG with ENOSYS.
        work(None)
    }
}

/// Only an unavailable SO_PEERPIDFD option permits the unconfigured route.
/// Errors creating the socketpair never pass through this classifier.
fn peer_pidfd_option_error(error: std::io::Error) -> std::io::Result<bool> {
    if error.raw_os_error() == Some(libc::ENOPROTOOPT) {
        Ok(false)
    } else {
        Err(error)
    }
}

fn probe_peer_pidfd() -> std::io::Result<bool> {
    let mut raw_pair = [-1; 2];
    // SAFETY: raw_pair is a writable two-descriptor output. These pristine
    // local endpoints carry no guest descriptors, messages, or protocol state.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            raw_pair.as_mut_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: successful socketpair installed two new descriptors owned here.
    let pair = unsafe {
        [
            OwnedFd::from_raw_fd(raw_pair[0]),
            OwnedFd::from_raw_fd(raw_pair[1]),
        ]
    };
    let mut raw_pidfd = -1;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SO_PEERPIDFD is the existing libc Linux ABI constant (77). The result is
    // discarded: it demonstrates this option, not the launcher's quiescence or
    // any later job client's identity. Workers still obtain their own pidfds.
    // SAFETY: both local output objects have the size declared to getsockopt.
    if unsafe {
        libc::getsockopt(
            pair[0].as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut raw_pidfd as *mut libc::c_int).cast(),
            &mut length,
        )
    } != 0
    {
        return peer_pidfd_option_error(std::io::Error::last_os_error());
    }
    if raw_pidfd < 0 || pair.iter().any(|fd| fd.as_raw_fd() == raw_pidfd) {
        return Err(std::io::Error::from_raw_os_error(libc::EPROTO));
    }
    // SAFETY: successful SO_PEERPIDFD returns a new owned descriptor, distinct
    // from the socketpair. Own it before validating length so a malformed
    // successful reply with a real descriptor cannot leak that descriptor.
    let _pidfd = unsafe { OwnedFd::from_raw_fd(raw_pidfd) };
    if length as usize != std::mem::size_of::<libc::c_int>() {
        return Err(std::io::Error::from_raw_os_error(libc::EPROTO));
    }
    Ok(true)
}

/// Defensive retention after the abort API refuses the exact creator identity.
/// No hard bound is established for this contract-violation path; a supervisor
/// must decide how to stop it. No incomplete wait becomes success.
fn retain_original_thread(owner: BrokerOwner, cause: &str) -> ! {
    let retained = ManuallyDrop::new(owner);
    let report = std::panic::catch_unwind(AssertUnwindSafe(|| {
        eprintln!(
            "HERMIT_CLEANUP_UNCONFIRMED: native broker pid={} retained on original thread: {cause}; stop this invocation through its supervisor",
            retained.native_pid()
        )
    }));
    let _retained_report = ManuallyDrop::new(report);
    loop {
        std::thread::park();
    }
}

fn poll_interest(mut interest: libc::pollfd) -> Result<(), Error> {
    loop {
        let rc = unsafe { libc::poll(&mut interest, 1, -1) };
        if rc >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error.into());
        }
    }
}

fn waited_status(status: i32) -> Result<(), Error> {
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        Ok(())
    } else {
        anyhow::bail!("native exit broker actual wait status {status}");
    }
}

enum Settlement {
    Reaped(Result<(), Error>),
    Unconfirmed(Error),
}

fn settle(owner: &mut BrokerOwner) -> Settlement {
    loop {
        match owner.request_shutdown() {
            Ok(true) => break,
            Ok(false) => {
                if let Err(error) = poll_interest(owner.shutdown_poll_interest()) {
                    return Settlement::Unconfirmed(error.context("shutdown poll"));
                }
            }
            Err(error) => match owner.try_wait() {
                Ok(Some(status)) => {
                    return Settlement::Reaped(Err(anyhow::anyhow!(
                        "native broker shutdown: {error:?}; actual wait status {status}"
                    )));
                }
                observed => {
                    return Settlement::Unconfirmed(anyhow::anyhow!(
                        "shutdown {error:?}; wait {observed:?}"
                    ));
                }
            },
        }
    }
    loop {
        match owner.try_wait() {
            Ok(Some(status)) => return Settlement::Reaped(waited_status(status)),
            Ok(None) => match owner.exit_poll_interest() {
                Some(interest) => {
                    if let Err(error) = poll_interest(interest) {
                        return Settlement::Unconfirmed(error.context("exit poll"));
                    }
                }
                None => {
                    return Settlement::Unconfirmed(anyhow::anyhow!(
                        "no authenticated exit poll handle"
                    ));
                }
            },
            Err(error) => {
                return Settlement::Unconfirmed(anyhow::anyhow!("actual broker wait: {error:?}"));
            }
        }
    }
}

/// Parent and child own different descriptor tables after the actual fork.
/// The parent copy stays in owned_container's retained guards and is never
/// used for protocol I/O. Only the child takes its copied owner for adoption.
pub(super) struct ForkHandoff {
    channel: Option<OwnedFd>,
    nonce: [u8; 32],
    enabled: bool,
    consumed: bool,
}

fn retain_exec_failure(failure: ExecClientFailure) -> Error {
    let error = anyhow::anyhow!("native exit client handoff: {failure:?}");
    // This happens before guest admission. Preserve the exact returned owner
    // and any unexpected rights until this process exits natively.
    let _retained = ManuallyDrop::new(failure);
    error
}

impl ForkHandoff {
    pub(super) fn export(owner: Option<&BrokerOwner>) -> Result<Self, Error> {
        let Some(owner) = owner else {
            return Ok(Self {
                channel: None,
                nonce: [0; 32],
                enabled: false,
                consumed: false,
            });
        };
        let (channel, nonce) = owner
            .export_client_for_exec()
            .map_err(retain_exec_failure)?
            .into_parts();
        Ok(Self {
            channel: Some(channel),
            nonce,
            enabled: true,
            consumed: false,
        })
    }

    /// Called exclusively by the owned_container child callback. No numeric
    /// PID test is used as authority across PID namespaces; the dedicated
    /// channel and server-first nonce authenticate this copied child owner.
    pub(super) fn adopt(&mut self) -> Result<Option<BrokerClient>, Error> {
        if !self.enabled {
            return Ok(None);
        }
        if self.consumed {
            anyhow::bail!("native exit handoff already consumed");
        }
        self.consumed = true;
        let channel = self
            .channel
            .take()
            .ok_or_else(|| anyhow::anyhow!("native exit handoff lost its descriptor owner"))?;
        BrokerClient::adopt_exec_channel(channel, self.nonce)
            .map(Some)
            .map_err(retain_exec_failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_entry_never_requests_startup_authority() {
        assert_eq!(
            with_early_owner(false, |owner| {
                assert!(owner.is_none());
                Ok(19)
            })
            .unwrap(),
            19
        );
    }

    #[test]
    fn disabled_entry_preserves_original_error_identity() {
        let error = with_early_owner::<()>(false, |owner| {
            assert!(owner.is_none());
            Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into())
        })
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::ENOSPC)
        );
    }

    #[test]
    fn absent_fork_handoff_preserves_unconfigured_admission() {
        let mut handoff = ForkHandoff::export(None).unwrap();
        assert!(handoff.adopt().unwrap().is_none());
        assert!(handoff.channel.is_none());
        assert!(!handoff.consumed);
    }

    #[test]
    fn native_wait_requires_zero_normal_exit() {
        assert!(waited_status(0).is_ok());
        assert!(waited_status(1 << 8).is_err());
        assert!(waited_status(libc::SIGKILL).is_err());
    }
    #[test]
    fn unavailable_peer_pidfd_runs_unconfigured_without_startup_authority() {
        let calls = std::cell::Cell::new(0);
        let result = with_optional_peer_pidfd_capability(
            || peer_pidfd_option_error(std::io::Error::from_raw_os_error(libc::ENOPROTOOPT)),
            |owner| {
                assert!(owner.is_none());
                calls.set(calls.get() + 1);
                Ok(23)
            },
            |_| panic!("unavailable capability must not attempt broker startup"),
        )
        .unwrap();
        assert_eq!(result, 23);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn peer_pidfd_probe_errors_preserve_errno_and_run_no_callback() {
        for errno in [libc::EPERM, libc::EMFILE, libc::EINTR, libc::EINVAL] {
            let called = std::cell::Cell::new(false);
            let attempted = std::cell::Cell::new(false);
            let error = with_optional_peer_pidfd_capability(
                || peer_pidfd_option_error(std::io::Error::from_raw_os_error(errno)),
                |_| {
                    called.set(true);
                    Ok(())
                },
                |_| {
                    attempted.set(true);
                    Ok(())
                },
            )
            .unwrap_err();
            assert!(!called.get());
            assert!(!attempted.get());
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(errno)
            );
        }
    }

    #[test]
    fn socketpair_error_does_not_claim_unavailable_peer_pidfd_option() {
        let error = with_optional_peer_pidfd_capability(
            || Err(std::io::Error::from_raw_os_error(libc::ENOPROTOOPT)),
            |_| -> Result<(), Error> { panic!("socketpair failure must not run guest work") },
            |_| panic!("socketpair failure must not attempt broker startup"),
        )
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::ENOPROTOOPT)
        );
    }

    #[test]
    fn available_peer_pidfd_never_falls_back_after_attempted_setup_failure() {
        let called = std::cell::Cell::new(false);
        let attempts = std::cell::Cell::new(0);
        let error = with_optional_peer_pidfd_capability(
            || Ok(true),
            |_| {
                called.set(true);
                Ok(())
            },
            |_| {
                attempts.set(attempts.get() + 1);
                Err(std::io::Error::from_raw_os_error(libc::EPERM).into())
            },
        )
        .unwrap_err();
        assert!(!called.get());
        assert_eq!(attempts.get(), 1);
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::EPERM)
        );
    }
}
