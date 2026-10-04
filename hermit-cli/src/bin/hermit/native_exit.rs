/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Ordinary OSS KVM CLI ownership. No broker is started by a library callback.
//! The original main thread owns and reaps it; each real container adopts a
//! dedicated channel. Nothing here changes guest scheduling or host signals.

use std::mem::ManuallyDrop;
use std::os::fd::OwnedFd;
use std::panic::AssertUnwindSafe;

use hermit::Error;
use reverie_kvm::native_exit_broker::BrokerClient;
use reverie_kvm::native_exit_broker::BrokerOwner;
use reverie_kvm::native_exit_broker::ExecClientFailure;
use reverie_kvm::native_exit_broker::StartupAuthority;

/// This function is called only by the real synchronous main, after parsing
/// and before evidence/log/run setup. Libtest and library callers cannot
/// assert the early-main contract by entering a backend helper.
pub(super) fn with_early_owner<T>(
    enabled: bool,
    work: impl FnOnce(Option<&BrokerOwner>) -> Result<T, Error>,
) -> Result<T, Error> {
    if !enabled || !cfg!(hermit_oss_early_main) {
        // Unproved startup builds keep ordinary, unconfigured KVM working.
        // They do not claim parent-death support: producer nonzero SET refuses
        // without terminal cleanup. Only an attempted setup failure is fatal.
        return work(None);
    }
    // SAFETY: the Cargo/OSS main is synchronous and its pinned fbinit wrapper
    // starts no threads. This call precedes tracing, Tokio and guest setup.
    // No owner is captured by a child callback. This main thread stays alive,
    // without exec or credential/namespace changes, through the exact wait.
    let authority = unsafe { StartupAuthority::assert_exclusive_early_launch() };
    let owner = match BrokerOwner::bootstrap(authority) {
        Ok(owner) => owner,
        Err(failure) => {
            let primary = anyhow::anyhow!("native exit broker bootstrap: {failure:?}");
            // Unexpected received rights remain owned until native CLI exit;
            // do not ordinarily close an unclassified foreign reference.
            let mut retained = ManuallyDrop::new(failure);
            if let Some(owner) = retained.child.take()
                && let Err(cleanup) = settle(*owner)
            {
                return Err(primary.context(format!("bootstrap child cleanup: {cleanup:#}")));
            }
            return Err(primary);
        }
    };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| work(Some(&owner))));
    if super::owned_container::has_retained_owner() {
        retain_original_thread(owner, "container producer remains unresolved");
    }
    let settled = settle(owner);
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
}

/// Preserve the real owner on its creator thread. Existing outer CLI/manifest
/// supervision supplies the hard bound; no incomplete wait becomes success.
fn retain_original_thread(owner: BrokerOwner, cause: &str) -> ! {
    eprintln!(
        "HERMIT_CLEANUP_UNCONFIRMED: native broker pid={} retained on original thread: {cause}; stop this invocation through its supervisor",
        owner.native_pid()
    );
    let _retained = ManuallyDrop::new(owner);
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

fn settle(mut owner: BrokerOwner) -> Result<(), Error> {
    loop {
        match owner.request_shutdown() {
            Ok(true) => break,
            Ok(false) => {
                if let Err(error) = poll_interest(owner.shutdown_poll_interest()) {
                    retain_original_thread(owner, &format!("shutdown poll: {error:#}"));
                }
            }
            Err(error) => {
                // A dead daemon may close its socket before shutdown. Only a
                // real wait permits releasing the owner, never ECHILD alone.
                match owner.try_wait() {
                    Ok(Some(status)) => {
                        return Err(anyhow::anyhow!(
                            "native broker shutdown: {error:?}; actual wait status {status}"
                        ));
                    }
                    observed => retain_original_thread(
                        owner,
                        &format!("shutdown {error:?}; wait {observed:?}"),
                    ),
                }
            }
        }
    }
    loop {
        match owner.try_wait() {
            Ok(Some(status)) => return waited_status(status),
            Ok(None) => match owner.exit_poll_interest() {
                Some(interest) => {
                    if let Err(error) = poll_interest(interest) {
                        retain_original_thread(owner, &format!("exit poll: {error:#}"));
                    }
                }
                None => retain_original_thread(owner, "no authenticated exit poll handle"),
            },
            Err(error) => retain_original_thread(owner, &format!("actual broker wait: {error:?}")),
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
}
