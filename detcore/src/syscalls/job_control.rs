/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A terminal's job-control signals that stop a call Detcore had the backend
//! inject, before the call ran (`reverie::Tool::decide_injection_stop`).
//!
//! Linux raises `SIGTTIN` and `SIGTTOU` in one place, `__tty_check_change`
//! (drivers/tty/tty_jobctrl.c). It does so only for a call on the caller's
//! controlling terminal (`current->signal->tty == tty`) made from a
//! background process group, while the signal is neither ignored nor
//! blocked: `SIGTTIN` for a read (the line discipline's `job_control`), and
//! `SIGTTOU` for a write with `TOSTOP` set or a change of the terminal's
//! settings or foreground group (`tty_check_change`). It sends the signal to
//! the caller's whole process group (`kill_pgrp`, so with `SI_KERNEL`) and
//! fails the call with `-ERESTARTSYS`, so a retry of that call raises it
//! again. Every other member of the group gets the same signal in the middle
//! of whatever call it is making; a retry of that other call does not raise
//! it again.
//!
//! At its default disposition such a signal is a stop of the process group,
//! which Detcore cannot make (<https://github.com/rrnewton/hermit/issues/4944>,
//! minibeads dh-h2qd98). Held for the guest's resume, as the backend holds a
//! signal that stops an injected call before it ran, the signal would come
//! after the call's retry, which raises it again while the process stays in
//! the background, each new signal replacing the held one: the call would
//! never return. So [`decide`] refuses by name a call that could have raised
//! the signal itself. The backend fails that call with `EIO`, which Linux
//! gives a background read when `SIGTTIN` is ignored or blocked, and which no
//! retry repeats, and the refusal reaches the call's handler as the backend's
//! record (`Detcore::report_backend_refusal`,
//! `Detcore::refuse_job_control_stop`). A call that cannot raise the signal
//! gets it held like any other, and Detcore refuses it by name once it is
//! delivered (`Detcore::refuse_linux_raised_job_control_stop`).

use reverie::DescriptorFile;
use reverie::Errno;
use reverie::FactError;
use reverie::InjectionStop;
use reverie::InjectionStopFacts;
use reverie::InjectionStopOutcome;
use reverie::UnsupportedOperation;
use reverie::UnsupportedRefusal;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;

/// The signals Detcore decides when one stops an injected call before it ran
/// (`reverie::Tool::injection_stop_signals`): `SIGTTIN` and `SIGTTOU`, signal
/// `n` at bit `n - 1`.
pub(crate) const INJECTION_STOP_SIGNALS: u64 =
    (1 << (libc::SIGTTIN - 1)) | (1 << (libc::SIGTTOU - 1));

/// `/dev/tty` (5, 0), `/dev/console` (5, 1) and `/dev/tty0` (4, 0): a
/// descriptor open on one of them reaches a terminal through another device
/// number than the terminal's own. `/dev/tty` is the caller's controlling
/// terminal, whatever that terminal's number (`tty_open_current_tty`);
/// `/dev/tty0` is the foreground virtual console and `/dev/console` the
/// console (`tty_lookup_driver`), either of which can be the caller's
/// controlling terminal. They are counted whichever terminal they reach, so
/// more often than Linux stops a call through them: through `/dev/console`
/// Linux stops no read or write (n_tty's `job_control` and `n_tty_write` skip
/// `redirected_tty_write`), only a change of the settings of a console that
/// is the caller's controlling terminal, and through `/dev/tty0` only a call
/// where the foreground console is. Counting too many can only refuse a call
/// that did not raise the signal; counting too few would hold the signal for
/// a call that did, which raises it again on every retry.
const TERMINAL_ALIASES: [(u32, u32); 3] = [(5, 0), (5, 1), (4, 0)];

/// `/dev/ptmx` (5, 2), which every pseudoterminal master is open on. Linux
/// checks a change of a master's settings or foreground group against the
/// master's slave (`tty_mode_ioctl` and `tiocspgrp` take `tty->link`), which
/// can be the caller's controlling terminal, so an `ioctl` through a master
/// is counted, as the facts do not say which slave a master serves. A read or
/// a write through a master is checked against the master itself, which no
/// process has as its controlling terminal (`TIOCSCTTY` through a master
/// takes its slave, `tty_pair_get_tty`), so it is not counted.
const PSEUDOTERMINAL_MULTIPLEXER: (u32, u32) = (5, 2);

/// The descriptor through which `nr`, called with `args`, can reach a
/// terminal call that raises `signal`:
/// - for `SIGTTIN`, a read: `read`, `readv` and `preadv2`, and the input of
///   `sendfile` and `splice`;
/// - for `SIGTTOU`, a write or a change of settings: `write`, `writev`,
///   `pwritev2` and `ioctl`, and the output of `sendfile` and `splice`.
///
/// A positioned `pread64`, `preadv`, `pwrite64` or `pwritev` of a terminal
/// fails with `ESPIPE` before it reaches the terminal, because Linux opens a
/// terminal unseekable (`nonseekable_open` in `tty_open`). `preadv2` and
/// `pwritev2` with offset -1 use the file position instead, so they are
/// included whatever their offset. `None` for any other call or signal.
pub(crate) fn descriptor(nr: Sysno, args: &SyscallArgs, signal: i32) -> Option<i32> {
    // The kernel takes each of these descriptors as a 32-bit int.
    let fd = |arg: usize| Some(arg as u32 as i32);
    match (signal, nr) {
        (libc::SIGTTIN, Sysno::read | Sysno::readv | Sysno::preadv2 | Sysno::splice) => {
            fd(args.arg0)
        }
        (libc::SIGTTIN, Sysno::sendfile) => fd(args.arg1),
        (libc::SIGTTOU, Sysno::write | Sysno::writev | Sysno::pwritev2 | Sysno::ioctl) => {
            fd(args.arg0)
        }
        (libc::SIGTTOU, Sysno::sendfile) => fd(args.arg0),
        (libc::SIGTTOU, Sysno::splice) => fd(args.arg2),
        _ => None,
    }
}

/// Whether call `nr` through a descriptor open on `file` can reach the
/// controlling terminal whose device number is `terminal` (as `stat`'s
/// `tty_nr` encodes it, which is how `stat(2)` encodes `file.rdev`): `file`
/// is a character device that is that terminal, one of [`TERMINAL_ALIASES`],
/// or, for an `ioctl`, [`PSEUDOTERMINAL_MULTIPLEXER`].
pub(crate) fn reaches_the_terminal(nr: Sysno, file: DescriptorFile, terminal: u32) -> bool {
    let is = |(major, minor): (u32, u32)| file.rdev == libc::makedev(major, minor);
    file.mode & libc::S_IFMT == libc::S_IFCHR
        && (file.rdev == u64::from(terminal)
            || TERMINAL_ALIASES.into_iter().any(is)
            || (nr == Sysno::ioctl && is(PSEUDOTERMINAL_MULTIPLEXER)))
}

/// Whether an injected call could have raised a terminal's job-control signal
/// itself ([`call_may_raise`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MayRaise {
    /// It could have, or it is not known that it could not.
    Yes,
    /// It could not.
    No,
    /// The thread no longer lives.
    Gone,
}

/// Whether `call`, which `signal`, a terminal's job-control signal, stopped
/// before it ran, could have raised that signal itself: a call [`descriptor`]
/// names, on a descriptor that reaches the thread's controlling terminal
/// ([`reaches_the_terminal`]). The facts are read in that order, and only as
/// far as the answer needs them. A call that is not known, or whose answer
/// cannot be read, could have: the decision then fails closed rather than
/// hold a signal that each retry of the call would raise again.
pub(crate) fn call_may_raise(
    call: Option<(Sysno, SyscallArgs)>,
    signal: i32,
    facts: &dyn InjectionStopFacts,
) -> MayRaise {
    let Some((nr, args)) = call else {
        return MayRaise::Yes;
    };
    let Some(fd) = descriptor(nr, &args, signal) else {
        return MayRaise::No;
    };
    let terminal = match facts.controlling_terminal() {
        // Without a controlling terminal, no call raises the signal.
        Ok(0) => return MayRaise::No,
        Ok(terminal) => terminal,
        Err(FactError::Errno(Errno::ESRCH)) => return MayRaise::Gone,
        Err(error) => {
            tracing::warn!(
                "signal {signal} stopped an injected {nr} before it ran, and the thread's \
                 controlling terminal could not be read: {error:?}"
            );
            return MayRaise::Yes;
        }
    };
    match facts.descriptor_file(fd) {
        Ok(file) if reaches_the_terminal(nr, file, terminal) => MayRaise::Yes,
        Ok(_) | Err(FactError::Errno(Errno::EBADF)) => MayRaise::No,
        Err(FactError::Errno(Errno::ESRCH)) => MayRaise::Gone,
        Err(error) => {
            tracing::warn!(
                "signal {signal} stopped an injected {nr} before it ran, and the file its \
                 descriptor {fd} is open on could not be read: {error:?}"
            );
            MayRaise::Yes
        }
    }
}

/// Why [`decide`] refuses a call: the stop itself, or a fact the decision
/// needs that cannot be read. Each is refused by name rather than held, since
/// a held signal of a stop Detcore cannot make brings the call back to raise
/// it again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Reason {
    /// Linux raised the signal (`SI_KERNEL`) at its default disposition.
    Stop,
    /// The thread's signal dispositions could not be read.
    DispositionUnread,
    /// The disposition is the default, and the backend does not report the
    /// siginfo, which tells a raised signal from a sent one.
    OriginNotProvided,
    /// The disposition is the default, and the siginfo could not be read.
    OriginUnread,
}

/// The diagnostic of [`decide`]'s refusal of `SIGTTIN` (`ttin`) or `SIGTTOU`
/// for `reason`.
fn diagnostic(ttin: bool, reason: Reason) -> &'static str {
    match (ttin, reason) {
        (true, Reason::Stop) => {
            "Linux raised SIGTTIN, at its default disposition, during the call (a background \
             process's read of its controlling terminal), and Detcore cannot stop the process \
             group for it; job-control stops are not supported"
        }
        (false, Reason::Stop) => {
            "Linux raised SIGTTOU, at its default disposition, during the call (a background \
             process's write to its controlling terminal with TOSTOP set, or a change of the \
             terminal's settings), and Detcore cannot stop the process group for it; \
             job-control stops are not supported"
        }
        (true, Reason::DispositionUnread) => {
            "SIGTTIN arrived during the call, and the thread's signal dispositions could not be \
             read, so Detcore cannot tell a job-control stop it cannot make (a background \
             process's read of its controlling terminal) from a handled signal; job-control \
             stops are not supported"
        }
        (false, Reason::DispositionUnread) => {
            "SIGTTOU arrived during the call, and the thread's signal dispositions could not be \
             read, so Detcore cannot tell a job-control stop it cannot make (a background \
             process's write to its controlling terminal) from a handled signal; job-control \
             stops are not supported"
        }
        (true, Reason::OriginNotProvided) => {
            "SIGTTIN, at its default disposition, arrived during the call, and the backend does \
             not report its siginfo, so Detcore cannot tell whether Linux raised it (a \
             background process's read of its controlling terminal) or a process sent it; \
             job-control stops are not supported"
        }
        (false, Reason::OriginNotProvided) => {
            "SIGTTOU, at its default disposition, arrived during the call, and the backend does \
             not report its siginfo, so Detcore cannot tell whether Linux raised it (a \
             background process's write to its controlling terminal) or a process sent it; \
             job-control stops are not supported"
        }
        (true, Reason::OriginUnread) => {
            "SIGTTIN, at its default disposition, arrived during the call, and its siginfo \
             could not be read, so Detcore cannot tell whether Linux raised it (a background \
             process's read of its controlling terminal) or a process sent it; job-control \
             stops are not supported"
        }
        (false, Reason::OriginUnread) => {
            "SIGTTOU, at its default disposition, arrived during the call, and its siginfo \
             could not be read, so Detcore cannot tell whether Linux raised it (a background \
             process's write to its controlling terminal) or a process sent it; job-control \
             stops are not supported"
        }
    }
}

/// The `si_code` of a siginfo, as the kernel's `siginfo_t` lays it out: the
/// third `int`, after `si_signo` and `si_errno`.
fn si_code(info: &[u8; reverie::SIGNAL_INFO_SIZE]) -> i32 {
    i32::from_ne_bytes([info[8], info[9], info[10], info[11]])
}

/// Detcore's answer for `stop`, a signal that stopped an injected call before
/// the call ran (`reverie::Tool::decide_injection_stop`), from the facts it
/// reads through `facts` while the thread stays stopped. A terminal's
/// `SIGTTIN` or `SIGTTOU` that Linux raised (`SI_KERNEL`) at its default
/// disposition, for a call that could have raised it itself
/// ([`call_may_raise`]), is refused with `EIO`
/// (`reverie::UnsupportedOperation::JobControlStop`); a fact the decision
/// needs that cannot be read is refused by name too, with its own
/// diagnostic. Any other signal is held, as is one for a call that cannot
/// raise it, one the thread ignores or catches, one a process sent, and one
/// for a thread that no longer lives.
///
/// The call is checked first, so a group's signal stopping a call that
/// cannot raise it never costs that call its result. A call that could have
/// raised it is refused even where another member's call raised it: every
/// `ioctl` counts, getters included, and so do the device aliases, whichever
/// terminal they reach ([`TERMINAL_ALIASES`]); that refuses too often rather
/// than hold a signal the call raises again. `SIGTSTP` is not decided,
/// although a terminal's suspend character raises it with `SI_KERNEL` too:
/// no call raises it, so the restarted call does not raise it again, and it
/// is held like any other signal.
pub(crate) fn decide(stop: &InjectionStop, facts: &dyn InjectionStopFacts) -> InjectionStopOutcome {
    let ttin = match stop.signal {
        libc::SIGTTIN => true,
        libc::SIGTTOU => false,
        _ => return InjectionStopOutcome::Hold,
    };
    if call_may_raise(stop.call, stop.signal, facts) != MayRaise::Yes {
        return InjectionStopOutcome::Hold;
    }
    let bit = 1u64 << (stop.signal - 1);
    let reason = match facts.signal_sets() {
        Err(FactError::Errno(Errno::ESRCH)) => return InjectionStopOutcome::Hold,
        Err(error) => {
            tracing::warn!(
                "(tid = {}) signal {} stopped an injected call before it ran, and its \
                 dispositions could not be read: {error:?}",
                stop.tid,
                stop.signal
            );
            Reason::DispositionUnread
        }
        Ok(sets) if (sets.ignored | sets.caught) & bit != 0 => {
            return InjectionStopOutcome::Hold;
        }
        Ok(_) => match facts.signal_info() {
            Ok(info) if si_code(&info) == libc::SI_KERNEL => Reason::Stop,
            Ok(_) | Err(FactError::Errno(Errno::ESRCH)) => return InjectionStopOutcome::Hold,
            Err(FactError::NotProvided) => Reason::OriginNotProvided,
            Err(error) => {
                tracing::warn!(
                    "(tid = {}) signal {} stopped an injected call before it ran, and its \
                     siginfo could not be read: {error:?}",
                    stop.tid,
                    stop.signal
                );
                Reason::OriginUnread
            }
        },
    };
    InjectionStopOutcome::Refuse(UnsupportedRefusal::new(
        UnsupportedOperation::JobControlStop,
        Errno::EIO,
        diagnostic(ttin, reason),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use reverie::SignalSets;
    use reverie::Tid;

    use super::*;

    fn args(arg0: usize, arg1: usize, arg2: usize) -> SyscallArgs {
        SyscallArgs::new(arg0, arg1, arg2, 0, 0, 0)
    }

    fn bit(signal: i32) -> u64 {
        1 << (signal - 1)
    }

    fn character(major: u32, minor: u32) -> DescriptorFile {
        DescriptorFile::new(libc::S_IFCHR | 0o620, libc::makedev(major, minor))
    }

    /// A pseudoterminal slave, `/dev/pts/3`.
    const PTS: (u32, u32) = (136, 3);

    /// The facts of a thread, as a backend would read them, and the facts
    /// asked for, in order.
    struct Facts {
        terminal: Result<u32, FactError>,
        /// Descriptor 3's file; every other descriptor is not open.
        three: Result<DescriptorFile, FactError>,
        sets: Result<SignalSets, FactError>,
        si_code: Result<i32, FactError>,
        asked: Mutex<Vec<&'static str>>,
    }

    impl Facts {
        /// A thread whose controlling terminal is [`PTS`], with descriptor 3
        /// open on it, both signals at their defaults, and a signal Linux
        /// raised.
        fn on_the_terminal() -> Self {
            Self {
                terminal: Ok(libc::makedev(PTS.0, PTS.1) as u32),
                three: Ok(character(PTS.0, PTS.1)),
                sets: Ok(SignalSets::new(0, 0, 0, 0, 0)),
                si_code: Ok(libc::SI_KERNEL),
                asked: Mutex::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<&'static str> {
            self.asked.lock().unwrap().clone()
        }

        fn note(&self, fact: &'static str) {
            self.asked.lock().unwrap().push(fact);
        }
    }

    impl InjectionStopFacts for Facts {
        fn signal_info(&self) -> Result<[u8; reverie::SIGNAL_INFO_SIZE], FactError> {
            self.note("signal_info");
            self.si_code.map(|code| {
                let mut info = [0u8; reverie::SIGNAL_INFO_SIZE];
                info[8..12].copy_from_slice(&code.to_ne_bytes());
                info
            })
        }

        fn signal_sets(&self) -> Result<SignalSets, FactError> {
            self.note("signal_sets");
            self.sets
        }

        fn controlling_terminal(&self) -> Result<u32, FactError> {
            self.note("controlling_terminal");
            self.terminal
        }

        fn descriptor_file(&self, fd: i32) -> Result<DescriptorFile, FactError> {
            self.note("descriptor_file");
            if fd == 3 {
                self.three
            } else {
                Err(FactError::Errno(Errno::EBADF))
            }
        }
    }

    fn stop(nr: Sysno, signal: i32) -> InjectionStop {
        InjectionStop::new(Tid::from_raw(7), Some((nr, args(3, 4, 5))), signal)
    }

    fn refused(ttin: bool, reason: Reason) -> InjectionStopOutcome {
        InjectionStopOutcome::Refuse(UnsupportedRefusal::new(
            UnsupportedOperation::JobControlStop,
            Errno::EIO,
            diagnostic(ttin, reason),
        ))
    }

    /// A read raises SIGTTIN and a write or an ioctl raises SIGTTOU, each
    /// through its own descriptor argument; nothing else does.
    #[test]
    fn each_signal_has_its_calls_and_descriptor() {
        let three = args(3, 4, 5);
        for nr in [Sysno::read, Sysno::readv, Sysno::preadv2, Sysno::splice] {
            assert_eq!(descriptor(nr, &three, libc::SIGTTIN), Some(3), "{nr}");
            assert_eq!(
                descriptor(nr, &three, libc::SIGTTOU),
                (nr == Sysno::splice).then_some(5),
                "{nr}"
            );
        }
        assert_eq!(descriptor(Sysno::sendfile, &three, libc::SIGTTIN), Some(4));
        assert_eq!(descriptor(Sysno::sendfile, &three, libc::SIGTTOU), Some(3));
        for nr in [Sysno::write, Sysno::writev, Sysno::pwritev2, Sysno::ioctl] {
            assert_eq!(descriptor(nr, &three, libc::SIGTTOU), Some(3), "{nr}");
            assert_eq!(descriptor(nr, &three, libc::SIGTTIN), None, "{nr}");
        }
        for nr in [
            Sysno::pread64,
            Sysno::preadv,
            Sysno::pwrite64,
            Sysno::pwritev,
            Sysno::wait4,
            Sysno::ppoll,
            Sysno::rt_sigsuspend,
            Sysno::futex,
        ] {
            assert_eq!(descriptor(nr, &three, libc::SIGTTIN), None, "{nr}");
            assert_eq!(descriptor(nr, &three, libc::SIGTTOU), None, "{nr}");
        }
        assert_eq!(descriptor(Sysno::read, &three, libc::SIGTSTP), None);
        // A descriptor argument is the kernel's 32-bit int.
        let wide = args(0x1_0000_0007, 0, 0);
        assert_eq!(descriptor(Sysno::read, &wide, libc::SIGTTIN), Some(7));
    }

    /// The device must be a character device: the controlling terminal
    /// itself, `/dev/tty`, `/dev/console` or `/dev/tty0`, or, for an ioctl
    /// only, a pseudoterminal master.
    #[test]
    fn only_the_terminal_its_aliases_and_a_masters_ioctl_count() {
        let terminal = libc::makedev(PTS.0, PTS.1) as u32;
        for nr in [Sysno::read, Sysno::write, Sysno::ioctl] {
            for device in [PTS, (5, 0), (5, 1), (4, 0)] {
                assert!(
                    reaches_the_terminal(nr, character(device.0, device.1), terminal),
                    "{nr} through {device:?}"
                );
            }
            // Another terminal, another virtual console, a pipe, or a block
            // device with the terminal's number.
            for file in [
                character(136, 4),
                character(4, 1),
                DescriptorFile::new(libc::S_IFIFO | 0o600, 0),
                DescriptorFile::new(libc::S_IFBLK | 0o660, libc::makedev(PTS.0, PTS.1)),
            ] {
                assert!(
                    !reaches_the_terminal(nr, file, terminal),
                    "{nr} through {file:?}"
                );
            }
            assert_eq!(
                reaches_the_terminal(nr, character(5, 2), terminal),
                nr == Sysno::ioctl,
                "{nr} through a master"
            );
        }
        // A minor of 2^19 or more sets the top bit of the encoded number.
        let high = libc::makedev(136, 1 << 19);
        assert!(high as u32 >= 1 << 31);
        assert!(reaches_the_terminal(
            Sysno::read,
            DescriptorFile::new(libc::S_IFCHR | 0o620, high),
            high as u32
        ));
    }

    /// A call without a terminal descriptor for the signal could not have
    /// raised it, and needs no fact; a call that is not known could have.
    #[test]
    fn a_call_could_have_raised_it_only_through_a_terminal_descriptor() {
        let facts = Facts::on_the_terminal();
        assert_eq!(call_may_raise(None, libc::SIGTTIN, &facts), MayRaise::Yes);
        for nr in [
            Sysno::poll,
            Sysno::ppoll,
            Sysno::wait4,
            Sysno::futex,
            Sysno::pread64,
        ] {
            for signal in [libc::SIGTTIN, libc::SIGTTOU] {
                let call = Some((nr, args(3, 4, 5)));
                assert_eq!(call_may_raise(call, signal, &facts), MayRaise::No, "{nr}");
            }
        }
        let read = Some((Sysno::read, args(3, 4, 5)));
        let write = Some((Sysno::write, args(3, 4, 5)));
        assert_eq!(call_may_raise(read, libc::SIGTTOU, &facts), MayRaise::No);
        assert_eq!(call_may_raise(write, libc::SIGTTIN, &facts), MayRaise::No);
        assert!(facts.asked().is_empty(), "{:?}", facts.asked());
        assert_eq!(call_may_raise(read, libc::SIGTTIN, &facts), MayRaise::Yes);
        assert_eq!(call_may_raise(write, libc::SIGTTOU, &facts), MayRaise::Yes);
        assert_eq!(
            facts.asked(),
            ["controlling_terminal", "descriptor_file"].repeat(2)
        );
    }

    /// A read of a pipe, of `/dev/null` or of a descriptor that is not open
    /// could not have raised the signal, while the thread has a controlling
    /// terminal; with no controlling terminal, not even a read of a terminal
    /// could have.
    #[test]
    fn a_call_off_the_controlling_terminal_could_not_have_raised_it() {
        let read = Some((Sysno::read, args(3, 4, 5)));
        let elsewhere = Some((Sysno::read, args(4, 4, 5)));
        for three in [
            Ok(DescriptorFile::new(libc::S_IFIFO | 0o600, 0)),
            Ok(character(1, 3)),
            Err(FactError::Errno(Errno::EBADF)),
        ] {
            let facts = Facts {
                three,
                ..Facts::on_the_terminal()
            };
            assert_eq!(
                call_may_raise(read, libc::SIGTTIN, &facts),
                MayRaise::No,
                "{three:?}"
            );
        }
        let facts = Facts::on_the_terminal();
        assert_eq!(
            call_may_raise(elsewhere, libc::SIGTTIN, &facts),
            MayRaise::No
        );
        let facts = Facts {
            terminal: Ok(0),
            ..Facts::on_the_terminal()
        };
        assert_eq!(call_may_raise(read, libc::SIGTTIN, &facts), MayRaise::No);
        assert_eq!(facts.asked(), ["controlling_terminal"]);
    }

    /// A fact that cannot be read leaves the call one that could have raised
    /// the signal, and a thread that no longer lives raised nothing.
    #[test]
    fn an_unread_fact_could_have_and_a_gone_thread_did_not() {
        let read = Some((Sysno::read, args(3, 4, 5)));
        for (terminal, three, answer) in [
            (
                Err(FactError::Errno(Errno::EIO)),
                Ok(character(PTS.0, PTS.1)),
                MayRaise::Yes,
            ),
            (
                Err(FactError::NotProvided),
                Ok(character(1, 3)),
                MayRaise::Yes,
            ),
            (
                Ok(libc::makedev(PTS.0, PTS.1) as u32),
                Err(FactError::Errno(Errno::EACCES)),
                MayRaise::Yes,
            ),
            (
                Err(FactError::Errno(Errno::ESRCH)),
                Ok(character(PTS.0, PTS.1)),
                MayRaise::Gone,
            ),
            (
                Ok(libc::makedev(PTS.0, PTS.1) as u32),
                Err(FactError::Errno(Errno::ESRCH)),
                MayRaise::Gone,
            ),
        ] {
            let facts = Facts {
                terminal,
                three,
                ..Facts::on_the_terminal()
            };
            assert_eq!(
                call_may_raise(read, libc::SIGTTIN, &facts),
                answer,
                "{terminal:?} {three:?}"
            );
        }
    }

    /// Only a signal Linux raised at its default disposition, for a call that
    /// could have raised it, is refused, naming the signal; a sent one, an
    /// ignored or caught one, one for a call that cannot raise it, and any
    /// other signal are held, and the facts are read only as far as the
    /// answer needs them.
    #[test]
    fn only_a_raised_stop_at_its_default_is_refused() {
        for (nr, signal, ttin) in [
            (Sysno::read, libc::SIGTTIN, true),
            (Sysno::ioctl, libc::SIGTTOU, false),
        ] {
            let facts = Facts::on_the_terminal();
            assert_eq!(
                decide(&stop(nr, signal), &facts),
                refused(ttin, Reason::Stop),
                "{nr}"
            );
            assert_eq!(
                facts.asked(),
                [
                    "controlling_terminal",
                    "descriptor_file",
                    "signal_sets",
                    "signal_info"
                ]
            );
            let held = [
                Facts {
                    si_code: Ok(libc::SI_USER),
                    ..Facts::on_the_terminal()
                },
                Facts {
                    sets: Ok(SignalSets::new(0, 0, 0, bit(signal), 0)),
                    ..Facts::on_the_terminal()
                },
                Facts {
                    sets: Ok(SignalSets::new(0, 0, 0, 0, bit(signal))),
                    ..Facts::on_the_terminal()
                },
                Facts {
                    three: Ok(character(1, 3)),
                    ..Facts::on_the_terminal()
                },
            ];
            for facts in held {
                assert_eq!(
                    decide(&stop(nr, signal), &facts),
                    InjectionStopOutcome::Hold,
                    "{nr}: {:?} {:?} {:?}",
                    facts.si_code,
                    facts.sets,
                    facts.three
                );
            }
        }
        let facts = Facts::on_the_terminal();
        for signal in [libc::SIGTSTP, libc::SIGSTOP, libc::SIGUSR1] {
            assert_eq!(
                decide(&stop(Sysno::read, signal), &facts),
                InjectionStopOutcome::Hold
            );
        }
        assert!(facts.asked().is_empty(), "{:?}", facts.asked());
    }

    /// A fact the decision needs that cannot be read is refused by name, each
    /// with its own diagnostic, rather than held; a thread that no longer
    /// lives is held, as there is no stop to make.
    #[test]
    fn an_unread_fact_is_refused_by_name() {
        for (signal, ttin) in [(libc::SIGTTIN, true), (libc::SIGTTOU, false)] {
            let nr = if ttin { Sysno::read } else { Sysno::write };
            for (facts, answer) in [
                (
                    Facts {
                        sets: Err(FactError::Errno(Errno::EIO)),
                        ..Facts::on_the_terminal()
                    },
                    refused(ttin, Reason::DispositionUnread),
                ),
                (
                    Facts {
                        si_code: Err(FactError::NotProvided),
                        ..Facts::on_the_terminal()
                    },
                    refused(ttin, Reason::OriginNotProvided),
                ),
                (
                    Facts {
                        si_code: Err(FactError::Errno(Errno::EPERM)),
                        ..Facts::on_the_terminal()
                    },
                    refused(ttin, Reason::OriginUnread),
                ),
                (
                    Facts {
                        sets: Err(FactError::Errno(Errno::ESRCH)),
                        ..Facts::on_the_terminal()
                    },
                    InjectionStopOutcome::Hold,
                ),
                (
                    Facts {
                        si_code: Err(FactError::Errno(Errno::ESRCH)),
                        ..Facts::on_the_terminal()
                    },
                    InjectionStopOutcome::Hold,
                ),
                (
                    Facts {
                        terminal: Err(FactError::Errno(Errno::ESRCH)),
                        ..Facts::on_the_terminal()
                    },
                    InjectionStopOutcome::Hold,
                ),
            ] {
                assert_eq!(
                    decide(&stop(nr, signal), &facts),
                    answer,
                    "{nr}: {:?}",
                    facts.asked()
                );
            }
        }
    }
}
