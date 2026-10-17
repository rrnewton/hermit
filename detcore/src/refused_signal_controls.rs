/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Guest signal controls an in-guest runtime refused before Detcore saw the
//! call, reported once per run.
//!
//! On the in-guest backends the runtime answers a guest's handler-installing
//! `rt_sigaction`, and an alternate signal stack, with `EPERM` itself
//! (reverie-inguest `protect_runtime_control`): a guest handler would run
//! outside the Tool's view. Until A5 step 3c makes guest handlers supported,
//! each refusal is announced once per run, by name, so that a run whose guest
//! carries on after the `EPERM` (many do, Rust's standard library among them)
//! is not silently different from Linux.
//!
//! The runtime records each refusal; the runtime crate that hosts Detcore
//! installs the function that takes them ([`install`]); Detcore takes them at
//! each syscall entry and tells the coordinator ([`report`]), which prints
//! one WARN per refused signal and one for the alternate stack, per run. No
//! other backend installs a source, so nothing is reported there.

use std::sync::OnceLock;

use reverie::Guest;

use crate::Detcore;
use crate::record_or_replay::RecordOrReplay;

/// What a runtime refused since it was last asked: the signals whose new
/// action it refused, bit `n - 1` for signal `n`, and whether it refused an
/// alternate signal stack.
pub type RefusedSignalControls = (u64, bool);

static SOURCE: OnceLock<fn() -> RefusedSignalControls> = OnceLock::new();

/// Installs `source`, which takes the signal controls the runtime refused
/// since it was last called. The runtime crate calls this once per process
/// image; a forked child inherits it. Returns false if one was installed
/// already.
pub fn install(source: fn() -> RefusedSignalControls) -> bool {
    SOURCE.set(source).is_ok()
}

/// Takes what the runtime refused since the last call: nothing where no
/// source is installed.
fn take() -> RefusedSignalControls {
    SOURCE.get().map_or((0, false), |source| source())
}

/// At a syscall's entry, tells the coordinator what the runtime refused since
/// the last report (`GlobalRequest::AnnounceRefusedSignalControls`). The
/// message carries no logical time and is answered before any accounting, so
/// whether a run reports changes neither its schedule nor its records.
pub(crate) async fn report<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let (actions, alternate_stack) = take();
    if actions == 0 && !alternate_stack {
        return;
    }
    crate::tool_global::announce_refused_signal_controls(guest, actions, alternate_stack).await;
}

/// The WARN for a signal whose handler the runtime refused.
pub(crate) fn handler_refused_warning(signum: i32) -> String {
    let name = signal_name(signum);
    format!(
        "HERMIT_SIGNAL_HANDLER_REFUSED signum={signum} ({name}): the guest asked for a handler \
         for it and received EPERM, so the handler was not installed: in-guest signal handlers \
         are unsupported until A5 step 3c (https://github.com/rrnewton/hermit/issues/4485). A \
         {name} the guest receives takes the action it had before."
    )
}

/// The WARN for an alternate signal stack the runtime refused.
pub(crate) fn alternate_stack_refused_warning() -> String {
    "HERMIT_SIGNAL_ALTSTACK_REFUSED: the guest asked for an alternate signal stack and \
     received EPERM, so it was not installed: in-guest signal handlers are unsupported until \
     A5 step 3c (https://github.com/rrnewton/hermit/issues/4485)."
        .to_string()
}

/// `signum`'s name: `SIGUSR1`, or `SIGRTMIN+n` for a realtime signal.
fn signal_name(signum: i32) -> String {
    match nix::sys::signal::Signal::try_from(signum) {
        Ok(signal) => signal.as_str().to_string(),
        Err(_) if signum >= libc::SIGRTMIN() => format!("SIGRTMIN+{}", signum - libc::SIGRTMIN()),
        Err(_) => format!("signal {signum}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_warnings_name_the_signal_the_errno_and_why() {
        let usr1 = handler_refused_warning(libc::SIGUSR1);
        assert!(usr1.starts_with("HERMIT_SIGNAL_HANDLER_REFUSED signum=10 (SIGUSR1): "));
        assert!(usr1.contains("received EPERM"));
        assert!(usr1.contains("in-guest signal handlers are unsupported until A5 step 3c"));
        assert!(handler_refused_warning(libc::SIGRTMIN() + 2).contains("(SIGRTMIN+2)"));
        let stack = alternate_stack_refused_warning();
        assert!(stack.starts_with("HERMIT_SIGNAL_ALTSTACK_REFUSED: "));
        assert!(stack.contains("received EPERM"));
        // No backend is named: the text is the same wherever it is printed.
        for text in [usr1, stack] {
            for backend in ["liteinst", "LiteInst", "in-guest-trap", "sabre", "SaBRe"] {
                assert!(!text.contains(backend), "{text}");
            }
        }
    }
}
