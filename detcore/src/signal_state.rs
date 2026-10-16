/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore's own copy of each guest's signal dispositions and masks.
//!
//! Audit step A5 (ai_docs/2026-10-16-a5-signal-delivery-design.md in the
//! dev-hermit parent; https://github.com/rrnewton/hermit/issues/4485) moves
//! Linux signal policy into Detcore, so that no Detcore decision depends on
//! reading the host's `/proc/<pid>/task/<tid>/status` for a guest thread.
//! That read is the guest's real state only on the ptrace backend.
//!
//! This module is the state and its transitions, as Linux performs them. It
//! holds no guest handle and makes no syscall: callers apply a transition
//! once the guest operation it describes has committed. Step 1 keeps the
//! state in shadow mode: it is recorded on every backend and, on ptrace,
//! compared with the kernel's, but no decision reads it yet.
//!
//! What is kept, and where Linux keeps the same thing:
//! - [`Dispositions`]: the 64 actions of one `struct sighand_struct`. Every
//!   task cloned with `CLONE_SIGHAND` shares one table, so a thread state
//!   holds it behind a shared reference, as it holds the descriptor table
//!   that `CLONE_FILES` shares. `CLONE_SIGHAND` requires `CLONE_VM`, so the
//!   tasks sharing a table also share the memory that reference lives in, on
//!   every backend.
//! - [`ThreadSignals`]: a task's `blocked` mask, and the masks its open
//!   handler frames will restore.
//!
//! Pending signals are not here: Detcore's existing queue owners keep them.

use std::sync::Arc;
use std::sync::Mutex;

use reverie::syscalls::Errno;
use serde::Deserialize;
use serde::Serialize;

/// A kernel signal set: bit `n - 1` is signal `n`, as `/proc` and
/// `rt_sigprocmask` use it.
pub(crate) type Sigset = u64;

/// The highest signal number on x86-64 Linux (`_NSIG`).
pub(crate) const MAX_SIGNAL: i32 = 64;

const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

/// `sa_flags` bits the kernel keeps and reports back on x86-64: Linux's
/// `UAPI_SA_FLAGS` (`include/linux/signal_types.h`). `do_sigaction` clears
/// every other bit of a new action before storing it, so userspace can detect
/// unsupported flags, for example `SA_UNSUPPORTED`.
const UAPI_SA_FLAGS: u64 = sa_flag(libc::SA_NOCLDSTOP)
    | sa_flag(libc::SA_NOCLDWAIT)
    | sa_flag(libc::SA_SIGINFO)
    | sa_flag(libc::SA_ONSTACK)
    | sa_flag(libc::SA_RESTART)
    | sa_flag(libc::SA_NODEFER)
    | sa_flag(libc::SA_RESETHAND)
    | sa_flag(SA_EXPOSE_TAGBITS)
    | sa_flag(SA_RESTORER);

/// An `sa_flags` bit as the kernel's `unsigned long` holds it. The libc crate
/// declares the bits as `c_int`, so `SA_RESETHAND` (0x80000000) is negative
/// and a plain `as u64` would set the upper 32 bits as well.
const fn sa_flag(flag: libc::c_int) -> u64 {
    flag as u32 as u64
}

/// Not exported by the libc crate for x86-64.
const SA_EXPOSE_TAGBITS: libc::c_int = 0x0000_0800;

/// Not exported by the libc crate for x86-64 (`asm/signal.h`).
const SA_RESTORER: libc::c_int = 0x0400_0000;

/// The bit of `signal` in a [`Sigset`], or 0 outside `1..=64`.
pub(crate) fn sigbit(signal: i32) -> Sigset {
    if (1..=MAX_SIGNAL).contains(&signal) {
        1 << (signal - 1)
    } else {
        0
    }
}

/// SIGKILL and SIGSTOP: never blocked, never caught, never ignored.
const UNBLOCKABLE: Sigset = (1 << (libc::SIGKILL - 1)) | (1 << (libc::SIGSTOP - 1));

/// One signal's action, as `struct kernel_sigaction` lays it out.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Action {
    /// `SIG_DFL` (0), `SIG_IGN` (1), or the handler's address.
    pub(crate) handler: u64,
    /// `sa_flags`, limited to [`UAPI_SA_FLAGS`].
    pub(crate) flags: u64,
    /// `sa_restorer`.
    pub(crate) restorer: u64,
    /// `sa_mask`, without SIGKILL and SIGSTOP.
    pub(crate) mask: Sigset,
}

impl Action {
    /// The action as `do_sigaction` stores it.
    pub(crate) fn normalized(self) -> Self {
        Action {
            flags: self.flags & UAPI_SA_FLAGS,
            mask: self.mask & !UNBLOCKABLE,
            ..self
        }
    }

    fn is_caught(&self) -> bool {
        self.handler != SIG_DFL && self.handler != SIG_IGN
    }
}

/// The actions of one disposition domain, indexed by signal number - 1.
/// Always 64 entries (a `Vec` only because serde does not derive arrays
/// that long).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Action>", into = "Vec<Action>")]
pub(crate) struct Dispositions {
    actions: Vec<Action>,
}

impl From<Dispositions> for Vec<Action> {
    fn from(dispositions: Dispositions) -> Self {
        dispositions.actions
    }
}

impl TryFrom<Vec<Action>> for Dispositions {
    type Error = String;

    fn try_from(actions: Vec<Action>) -> Result<Self, Self::Error> {
        if actions.len() == MAX_SIGNAL as usize {
            Ok(Dispositions { actions })
        } else {
            Err(format!(
                "a signal action table has {} entries, not {MAX_SIGNAL}",
                actions.len()
            ))
        }
    }
}

impl Dispositions {
    /// A freshly executed image's actions: after `execve` a signal is either
    /// ignored or at its default, so the ignored set alone describes them.
    pub(crate) fn after_exec(ignored: Sigset) -> Self {
        let mut actions = vec![Action::default(); MAX_SIGNAL as usize];
        for (index, action) in actions.iter_mut().enumerate() {
            if ignored & (1 << index) & !UNBLOCKABLE != 0 {
                action.handler = SIG_IGN;
            }
        }
        Dispositions { actions }
    }

    fn index(signal: i32) -> Result<usize, Errno> {
        if (1..=MAX_SIGNAL).contains(&signal) {
            Ok((signal - 1) as usize)
        } else {
            Err(Errno::EINVAL)
        }
    }

    /// `signal`'s current action.
    #[cfg(test)]
    pub(crate) fn action(&self, signal: i32) -> Result<Action, Errno> {
        Ok(self.actions[Self::index(signal)?])
    }

    /// `rt_sigaction(signal, new, &old)` after the size check, which the
    /// caller makes first, as the kernel does. Returns the old action, which
    /// is what the kernel copies out. SIGKILL and SIGSTOP can be queried but
    /// not changed.
    pub(crate) fn sigaction(&mut self, signal: i32, new: Option<Action>) -> Result<Action, Errno> {
        let index = Self::index(signal)?;
        if new.is_some() && sigbit(signal) & UNBLOCKABLE != 0 {
            return Err(Errno::EINVAL);
        }
        let old = self.actions[index];
        if let Some(new) = new {
            self.actions[index] = new.normalized();
        }
        Ok(old)
    }

    /// The action used to deliver `signal`, chosen when the signal is
    /// dequeued for delivery. `SA_RESETHAND` puts the handler back to
    /// `SIG_DFL` at this point, before the frame is built, so a frame that
    /// cannot be written still leaves the default installed (Linux
    /// `get_signal`).
    pub(crate) fn select_for_delivery(&mut self, signal: i32) -> Result<Action, Errno> {
        let index = Self::index(signal)?;
        let action = self.actions[index];
        if action.is_caught() && action.flags & sa_flag(libc::SA_RESETHAND) != 0 {
            self.actions[index].handler = SIG_DFL;
        }
        Ok(action)
    }

    /// `execve`'s reset (Linux `flush_signal_handlers` with
    /// `force_default` false): a caught signal goes back to its default, an
    /// ignored one stays ignored, and every action loses its flags, restorer
    /// and mask.
    pub(crate) fn reset_for_exec(&mut self) {
        for action in &mut self.actions {
            let handler = if action.handler == SIG_IGN {
                SIG_IGN
            } else {
                SIG_DFL
            };
            *action = Action {
                handler,
                ..Action::default()
            };
        }
    }

    /// The signals whose action is `SIG_IGN`, as `/proc`'s `SigIgn`.
    pub(crate) fn ignored(&self) -> Sigset {
        self.select(|action| action.handler == SIG_IGN)
    }

    /// The signals with a handler installed, as `/proc`'s `SigCgt`.
    pub(crate) fn caught(&self) -> Sigset {
        self.select(Action::is_caught)
    }

    fn select(&self, keep: impl Fn(&Action) -> bool) -> Sigset {
        self.actions
            .iter()
            .enumerate()
            .filter(|(_, action)| keep(action))
            .fold(0, |set, (index, _)| set | (1 << index))
    }
}

/// One thread's masks.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ThreadSignals {
    /// The signals blocked now (Linux `task->blocked`).
    pub(crate) blocked: Sigset,
    /// The `uc_sigmask` of each handler frame entered and not yet known to
    /// have returned, innermost last. A backend that reports `rt_sigreturn`
    /// pops one per return ([`ThreadSignals::sigreturn`]); where the kernel
    /// runs `rt_sigreturn` unseen, a return is recognised when the kernel's
    /// mask equals one of these ([`ThreadSignals::reconcile_returns`]).
    #[serde(default)]
    pub(crate) activations: Vec<Sigset>,
}

impl ThreadSignals {
    /// A thread whose mask is `blocked`.
    pub(crate) fn new(blocked: Sigset) -> Self {
        ThreadSignals {
            blocked: blocked & !UNBLOCKABLE,
            activations: Vec::new(),
        }
    }

    /// `rt_sigprocmask(how, set, &old)` after the size check. Returns the old
    /// mask, read before any change, which is what the kernel copies out.
    /// `how` is checked only when `set` is given.
    pub(crate) fn sigprocmask(&mut self, how: i32, set: Option<Sigset>) -> Result<Sigset, Errno> {
        let old = self.blocked;
        if let Some(set) = set {
            let set = set & !UNBLOCKABLE;
            self.blocked = match how {
                libc::SIG_BLOCK => old | set,
                libc::SIG_UNBLOCK => old & !set,
                libc::SIG_SETMASK => set,
                _ => return Err(Errno::EINVAL),
            };
        }
        Ok(old)
    }

    /// A handler for `signal` with `action` was entered, its frame written
    /// (Linux `signal_delivered`). The handler runs with `sa_mask` added and,
    /// without `SA_NODEFER`, `signal` itself; the frame saves the mask from
    /// before. A delivery during a temporary-mask wait (`ppoll`, `pselect6`,
    /// `epoll_pwait`, `rt_sigsuspend`) saves the wait's restore mask instead
    /// (Linux `sigmask_to_save`); that case is not modelled yet, and the
    /// kernel check reports it.
    pub(crate) fn enter_handler(&mut self, signal: i32, action: &Action) {
        self.activations.push(self.blocked);
        let mut blocked = self.blocked | action.mask;
        if action.flags & sa_flag(libc::SA_NODEFER) == 0 {
            blocked |= sigbit(signal);
        }
        self.blocked = blocked & !UNBLOCKABLE;
    }

    /// `rt_sigreturn` read `mask` from the frame and installed it.
    pub(crate) fn sigreturn(&mut self, mask: Sigset) {
        self.activations.pop();
        self.blocked = mask & !UNBLOCKABLE;
    }

    /// Accounts for handler returns the backend did not report, given the
    /// kernel's current mask `observed`. Returns false when `observed` is
    /// neither the copy's mask nor the saved mask of an open handler frame:
    /// the copy is then wrong. A match pops that frame and every frame inside
    /// it. Only an unedited return is recognised this way: a handler that
    /// rewrites its frame's `uc_sigmask` is reported as a mismatch, which is
    /// a limit of the check and never a silent repair.
    ///
    /// Bits in `ignore` are left out of every comparison: signals whose
    /// kernel state the backend itself changes. A recognised return installs
    /// the frame's mask as the copy holds it, those bits included.
    pub(crate) fn reconcile_returns(&mut self, observed: Sigset, ignore: Sigset) -> bool {
        let same = |mask: Sigset| mask & !ignore == observed & !ignore;
        if same(self.blocked) {
            return true;
        }
        match self.activations.iter().rposition(|&mask| same(mask)) {
            Some(frame) => {
                self.blocked = self.activations[frame];
                self.activations.truncate(frame);
                true
            }
            None => false,
        }
    }
}

/// `signal` was selected for delivery and, for a caught signal, its handler
/// frame written: the shared `dispositions` and the receiving thread's
/// `thread` change as Linux changes them. Returns the action used. An
/// ignored or default action changes no mask; `SA_RESETHAND` has reset the
/// handler in either case.
pub(crate) fn deliver(
    dispositions: &mut Dispositions,
    thread: &mut ThreadSignals,
    signal: i32,
) -> Result<Action, Errno> {
    let action = dispositions.select_for_delivery(signal)?;
    if action.is_caught() {
        thread.enter_handler(signal, &action);
    }
    Ok(action)
}

/// One thread's view of the signal state: its process's (more exactly, its
/// `CLONE_SIGHAND` group's) actions, shared with every thread of the group,
/// and its own masks.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SignalShadow {
    /// Shared by every task created with `CLONE_SIGHAND`.
    pub(crate) dispositions: Arc<Mutex<Dispositions>>,
    /// This thread's masks.
    pub(crate) thread: ThreadSignals,
}

impl SignalShadow {
    /// The first image's state, from the ignored set and mask it inherited.
    pub(crate) fn seeded(ignored: Sigset, blocked: Sigset) -> Self {
        SignalShadow {
            dispositions: Arc::new(Mutex::new(Dispositions::after_exec(ignored))),
            thread: ThreadSignals::new(blocked),
        }
    }

    /// The state of a task this thread creates with clone flags `flags`. The
    /// child starts with this thread's mask and no open handler frame. With
    /// `CLONE_SIGHAND` it shares the actions; otherwise it gets a copy, as
    /// Linux's `copy_sighand` makes.
    pub(crate) fn for_clone(&self, flags: u64) -> Self {
        let dispositions = if flags & libc::CLONE_SIGHAND as u64 != 0 {
            Arc::clone(&self.dispositions)
        } else {
            Arc::new(Mutex::new(self.dispositions.lock().unwrap().clone()))
        };
        SignalShadow {
            dispositions,
            thread: ThreadSignals::new(self.thread.blocked),
        }
    }

    /// This thread completed an `execve`. Its actions become a private copy
    /// first (Linux `unshare_sighand`), so a task that shared them keeps its
    /// own, and are then reset. The mask is kept.
    pub(crate) fn exec(&mut self) {
        let mut own = self.dispositions.lock().unwrap().clone();
        own.reset_for_exec();
        self.dispositions = Arc::new(Mutex::new(own));
        self.thread.activations.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler(address: u64, flags: libc::c_int, mask: Sigset) -> Action {
        Action {
            handler: address,
            flags: sa_flag(flags | SA_RESTORER),
            restorer: 0x7000,
            mask,
        }
    }

    #[test]
    fn exec_image_starts_ignored_or_default_and_never_ignores_sigkill() {
        let ignored = sigbit(libc::SIGPIPE) | sigbit(libc::SIGKILL);
        let table = Dispositions::after_exec(ignored);
        assert_eq!(table.ignored(), sigbit(libc::SIGPIPE));
        assert_eq!(table.caught(), 0);
    }

    #[test]
    fn sigaction_returns_old_and_stores_normalized() {
        let mut table = Dispositions::after_exec(0);
        let new = Action {
            handler: 0x4000,
            flags: sa_flag(libc::SA_SIGINFO | SA_RESTORER) | 0x400 | (1 << 40),
            restorer: 0x7000,
            mask: sigbit(libc::SIGUSR2) | UNBLOCKABLE,
        };
        assert_eq!(
            table.sigaction(libc::SIGUSR1, Some(new)),
            Ok(Action::default())
        );
        let stored = table.action(libc::SIGUSR1).unwrap();
        assert_eq!(
            stored.flags,
            sa_flag(libc::SA_SIGINFO | SA_RESTORER),
            "SA_UNSUPPORTED (0x400) and bits above 31 are not kept"
        );
        assert_eq!(stored.mask, sigbit(libc::SIGUSR2));
        assert_eq!(table.sigaction(libc::SIGUSR1, None), Ok(stored));
        assert_eq!(table.caught(), sigbit(libc::SIGUSR1));
    }

    #[test]
    fn sigaction_refuses_sigkill_changes_and_out_of_range_signals() {
        let mut table = Dispositions::after_exec(0);
        let new = handler(0x4000, 0, 0);
        assert_eq!(
            table.sigaction(libc::SIGKILL, Some(new)),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            table.sigaction(libc::SIGSTOP, Some(new)),
            Err(Errno::EINVAL)
        );
        assert_eq!(table.sigaction(libc::SIGKILL, None), Ok(Action::default()));
        assert_eq!(table.sigaction(0, None), Err(Errno::EINVAL));
        assert_eq!(table.sigaction(65, None), Err(Errno::EINVAL));
        assert_eq!(table.sigaction(64, None), Ok(Action::default()));
    }

    #[test]
    fn exec_reset_keeps_ignored_and_clears_everything_else() {
        let mut table = Dispositions::after_exec(0);
        table
            .sigaction(libc::SIGUSR1, Some(handler(0x4000, libc::SA_RESTART, 3)))
            .unwrap();
        table
            .sigaction(
                libc::SIGPIPE,
                Some(Action {
                    handler: SIG_IGN,
                    flags: sa_flag(libc::SA_RESTART),
                    restorer: 0x7000,
                    mask: 3,
                }),
            )
            .unwrap();
        table.reset_for_exec();
        assert_eq!(table.action(libc::SIGUSR1), Ok(Action::default()));
        assert_eq!(
            table.action(libc::SIGPIPE),
            Ok(Action {
                handler: SIG_IGN,
                ..Action::default()
            })
        );
    }

    #[test]
    fn resethand_resets_at_selection_and_keeps_flags() {
        let mut table = Dispositions::after_exec(0);
        let action = handler(0x4000, libc::SA_RESETHAND, 0);
        table.sigaction(libc::SIGUSR1, Some(action)).unwrap();
        assert_eq!(
            table.select_for_delivery(libc::SIGUSR1),
            Ok(action.normalized())
        );
        let after = table.action(libc::SIGUSR1).unwrap();
        assert_eq!(after.handler, SIG_DFL);
        assert_eq!(after.flags, action.normalized().flags);
    }

    #[test]
    fn sign_extended_flags_from_glibc_keep_only_the_kernel_bits() {
        // glibc widens its int sa_flags into the kernel's unsigned long, so a
        // guest's SA_RESETHAND arrives as 0xffffffff80000000.
        let widened = libc::SA_RESETHAND as i64 as u64;
        let mut table = Dispositions::after_exec(0);
        let new = Action {
            flags: widened | sa_flag(SA_RESTORER),
            ..handler(0x4000, 0, 0)
        };
        table.sigaction(libc::SIGUSR1, Some(new)).unwrap();
        assert_eq!(
            table.action(libc::SIGUSR1).unwrap().flags,
            0x8000_0000 | sa_flag(SA_RESTORER)
        );
    }

    #[test]
    fn sigprocmask_returns_the_old_mask_and_never_blocks_sigkill() {
        let mut thread = ThreadSignals::new(sigbit(libc::SIGUSR1));
        assert_eq!(
            thread.sigprocmask(libc::SIG_BLOCK, Some(sigbit(libc::SIGKILL) | 4)),
            Ok(sigbit(libc::SIGUSR1))
        );
        assert_eq!(thread.blocked, sigbit(libc::SIGUSR1) | 4);
        assert_eq!(
            thread.sigprocmask(libc::SIG_UNBLOCK, Some(4)),
            Ok(sigbit(libc::SIGUSR1) | 4)
        );
        assert_eq!(
            thread.sigprocmask(libc::SIG_SETMASK, Some(8)),
            Ok(sigbit(libc::SIGUSR1))
        );
        assert_eq!(thread.blocked, 8);
        assert_eq!(thread.sigprocmask(99, Some(1)), Err(Errno::EINVAL));
        assert_eq!(thread.blocked, 8, "a refused call changes nothing");
        assert_eq!(
            thread.sigprocmask(99, None),
            Ok(8),
            "how is ignored without a set"
        );
    }

    #[test]
    fn handler_entry_adds_sa_mask_and_the_signal_unless_nodefer() {
        let mut thread = ThreadSignals::new(1);
        thread.enter_handler(libc::SIGUSR1, &handler(0x4000, 0, sigbit(libc::SIGUSR2)));
        assert_eq!(
            thread.blocked,
            1 | sigbit(libc::SIGUSR1) | sigbit(libc::SIGUSR2)
        );
        let mut thread = ThreadSignals::new(1);
        thread.enter_handler(libc::SIGUSR1, &handler(0x4000, libc::SA_NODEFER, 0));
        assert_eq!(thread.blocked, 1);
    }

    #[test]
    fn sigreturn_cannot_block_sigkill() {
        let mut thread = ThreadSignals::new(0);
        thread.sigreturn(u64::MAX);
        assert_eq!(thread.blocked, !UNBLOCKABLE);
    }

    #[test]
    fn delivery_to_a_handler_applies_resethand_and_the_handler_mask() {
        let mut dispositions = Dispositions::after_exec(sigbit(libc::SIGPIPE));
        let mut thread = ThreadSignals::new(0);
        dispositions
            .sigaction(
                libc::SIGUSR1,
                Some(handler(0x4000, libc::SA_RESETHAND, sigbit(libc::SIGUSR2))),
            )
            .unwrap();
        let used = deliver(&mut dispositions, &mut thread, libc::SIGUSR1).unwrap();
        assert_eq!(used.handler, 0x4000);
        assert_eq!(
            thread.blocked,
            sigbit(libc::SIGUSR1) | sigbit(libc::SIGUSR2)
        );
        assert_eq!(dispositions.caught(), 0);
        let ignored = deliver(&mut dispositions, &mut thread, libc::SIGPIPE).unwrap();
        assert_eq!(ignored.handler, SIG_IGN);
        assert_eq!(
            thread.blocked,
            sigbit(libc::SIGUSR1) | sigbit(libc::SIGUSR2),
            "an ignored signal changes no mask"
        );
    }

    #[test]
    fn clone_sighand_shares_actions_and_fork_copies_them() {
        let parent = SignalShadow::seeded(0, 4);
        let thread = parent.for_clone(libc::CLONE_SIGHAND as u64 | libc::CLONE_VM as u64);
        let mut child = parent.for_clone(0);
        thread
            .dispositions
            .lock()
            .unwrap()
            .sigaction(libc::SIGUSR1, Some(handler(0x4000, 0, 0)))
            .unwrap();
        assert_eq!(
            parent.dispositions.lock().unwrap().caught(),
            sigbit(libc::SIGUSR1)
        );
        assert_eq!(child.dispositions.lock().unwrap().caught(), 0);
        assert_eq!(child.thread.blocked, 4, "the mask is inherited");
        child
            .thread
            .sigprocmask(libc::SIG_SETMASK, Some(0))
            .unwrap();
        assert_eq!(parent.thread.blocked, 4, "masks are per thread");
    }

    #[test]
    fn exec_unshares_shared_actions_before_resetting_them() {
        let parent = SignalShadow::seeded(0, 0);
        let mut sibling = parent.for_clone(libc::CLONE_SIGHAND as u64 | libc::CLONE_VM as u64);
        parent
            .dispositions
            .lock()
            .unwrap()
            .sigaction(libc::SIGUSR1, Some(handler(0x4000, 0, 0)))
            .unwrap();
        sibling
            .thread
            .enter_handler(libc::SIGUSR2, &handler(0x4000, 0, 0));
        sibling.exec();
        assert_eq!(sibling.dispositions.lock().unwrap().caught(), 0);
        assert!(
            sibling.thread.activations.is_empty(),
            "exec leaves no frame open"
        );
        assert_eq!(
            parent.dispositions.lock().unwrap().caught(),
            sigbit(libc::SIGUSR1),
            "the peer keeps its handler"
        );
    }

    #[test]
    fn shadow_round_trips_through_serde_with_all_64_actions() {
        let shadow = SignalShadow::seeded(sigbit(64), 2);
        let bytes = serde_json::to_vec(&shadow).unwrap();
        let back: SignalShadow = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            *back.dispositions.lock().unwrap(),
            *shadow.dispositions.lock().unwrap()
        );
        assert_eq!(back.thread, shadow.thread);
        assert_eq!(back.dispositions.lock().unwrap().ignored(), sigbit(64));
        let short = serde_json::to_vec(&vec![Action::default(); 63]).unwrap();
        assert!(serde_json::from_slice::<Dispositions>(&short).is_err());
    }

    #[test]
    fn unreported_returns_are_recognised_by_their_saved_masks() {
        let mut thread = ThreadSignals::new(1);
        thread.enter_handler(libc::SIGUSR1, &handler(0x4000, 0, 0));
        let outer = thread.blocked;
        thread.enter_handler(libc::SIGUSR2, &handler(0x4000, 0, 0));
        assert_eq!(thread.activations, vec![1, outer]);
        assert!(thread.reconcile_returns(thread.blocked, 0), "still inside");
        assert!(
            thread.reconcile_returns(outer, 0),
            "the inner handler returned"
        );
        assert_eq!(thread.activations, vec![1]);
        assert!(thread.reconcile_returns(1, 0), "the outer one too");
        assert!(thread.activations.is_empty());
        assert_eq!(thread.blocked, 1);
        assert!(
            !thread.reconcile_returns(4, 0),
            "an unexplained mask is a mismatch"
        );
        assert!(
            thread.reconcile_returns(1 | 4, 4),
            "an ignored bit is left out"
        );
        assert_eq!(
            thread.blocked, 1,
            "the copy keeps its own value of an ignored bit"
        );
    }

    #[test]
    fn two_returns_at_once_pop_both_frames() {
        let mut thread = ThreadSignals::new(0);
        thread.enter_handler(libc::SIGUSR1, &handler(0x4000, 0, 0));
        thread.enter_handler(libc::SIGUSR2, &handler(0x4000, 0, 0));
        assert!(thread.reconcile_returns(0, 0));
        assert!(thread.activations.is_empty());
    }
}
