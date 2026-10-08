/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Seccomp filters a test installs in the process that becomes Hermit, before
//! it executes Hermit, so that Hermit, every thread it starts and every guest
//! inherit them, as they would a container runtime's filter. A filter cannot
//! be removed once installed, and since Linux 4.8 it also applies to a system
//! call a tracer injects into a guest.

use std::os::unix::process::CommandExt;
use std::process::Command;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_NATIVE: u32 = 0xc000_003e; // AUDIT_ARCH_X86_64
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_NATIVE: u32 = 0xc000_00b7; // AUDIT_ARCH_AARCH64

/// `AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT`: the flags of the lstat (a
/// `newfstatat`) Detcore injects for each entry a guest's getdents lists. A
/// guest's own lstat passes `AT_SYMLINK_NOFOLLOW` alone, and Hermit's and the
/// dynamic loader's fstat pass `AT_EMPTY_PATH`, so a rule on these flags
/// reaches only the injected lookup.
#[allow(dead_code)]
pub const INJECTED_LOOKUP_FLAGS: u32 = (libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT) as u32;

/// One rule: system call `nr` whose arguments at the given indices have the
/// given low 32 bits gets `verdict`.
pub struct Rule {
    pub nr: libc::c_long,
    pub args: &'static [(u32, u32)],
    pub verdict: u32,
}

/// A seccomp program: the first rule that matches a call answers it, a call
/// no rule matches is allowed, and a call made through another architecture's
/// calling convention kills the process.
pub struct Filter {
    program: Vec<libc::sock_filter>,
    /// Some rule answers `SECCOMP_RET_USER_NOTIF`.
    notifies: bool,
}

impl Filter {
    pub fn new(rules: &[Rule]) -> Self {
        let statement = |code: u32, k: u32| libc::sock_filter {
            code: code as u16,
            jt: 0,
            jf: 0,
            k,
        };
        let jump_if_equal = |k: u32, jf: usize| libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            jt: 0,
            jf: u8::try_from(jf).expect("a rule fits a BPF jump"),
            k,
        };
        let load_word = |offset: u32| statement(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, offset);
        let ret = |verdict: u32| statement(libc::BPF_RET | libc::BPF_K, verdict);
        // struct seccomp_data: nr at offset 0, arch at offset 4, args[i] at
        // 16 + 8 * i, whose low 32 bits come first on a little-endian host.
        let mut program = vec![
            load_word(4),
            libc::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                jt: 1,
                jf: 0,
                k: AUDIT_ARCH_NATIVE,
            },
            ret(libc::SECCOMP_RET_KILL_PROCESS),
        ];
        for rule in rules {
            // After the number's jump: two instructions per argument and the
            // verdict; a mismatch skips them all, to the next rule.
            let mut rest = 2 * rule.args.len() + 1;
            program.push(load_word(0));
            program.push(jump_if_equal(u32::try_from(rule.nr).unwrap(), rest));
            for &(index, value) in rule.args {
                rest -= 2;
                program.push(load_word(16 + 8 * index));
                program.push(jump_if_equal(value, rest));
            }
            program.push(ret(rule.verdict));
        }
        program.push(ret(libc::SECCOMP_RET_ALLOW));
        Self {
            program,
            notifies: rules
                .iter()
                .any(|rule| rule.verdict == libc::SECCOMP_RET_USER_NOTIF),
        }
    }

    /// Install the filter in `command`'s child after `PR_SET_NO_NEW_PRIVS`,
    /// just before it executes. When a rule answers `SECCOMP_RET_USER_NOTIF`,
    /// the listener is left open without `FD_CLOEXEC` and nothing answers it,
    /// as a container supervisor that has stopped answering would: Hermit
    /// inherits the listener and holds it while it runs, and so does every
    /// process it starts, so a notified call waits until its caller is killed
    /// (with no listener left it would fail with `ENOSYS` instead).
    pub fn install_before_exec(self, command: &mut Command) {
        let Self { program, notifies } = self;
        let flags = if notifies {
            libc::SECCOMP_FILTER_FLAG_NEW_LISTENER
        } else {
            0
        };
        // SAFETY: prctl, seccomp and fcntl are async-signal-safe, and the
        // program was built before the fork; the kernel copies it, so nothing
        // is allocated after the fork.
        unsafe {
            command.pre_exec(move || {
                let fprog = libc::sock_fprog {
                    len: u16::try_from(program.len()).unwrap_or(u16::MAX),
                    filter: program.as_ptr() as *mut libc::sock_filter,
                };
                if libc::prctl(
                    libc::PR_SET_NO_NEW_PRIVS,
                    1 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                let installed = libc::syscall(
                    libc::SYS_seccomp,
                    libc::SECCOMP_SET_MODE_FILTER as libc::c_long,
                    flags as libc::c_long,
                    &fprog as *const libc::sock_fprog as libc::c_long,
                );
                if installed < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if notifies && libc::fcntl(installed as libc::c_int, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    /// Install the filter on every thread of the calling process
    /// (`SECCOMP_FILTER_FLAG_TSYNC`), after `PR_SET_NO_NEW_PRIVS`. The filter
    /// must not notify.
    #[allow(dead_code)]
    pub fn install_on_this_process(self) {
        assert!(!self.notifies, "a listener here would have no holder");
        let fprog = libc::sock_fprog {
            len: u16::try_from(self.program.len()).unwrap(),
            filter: self.program.as_ptr() as *mut libc::sock_filter,
        };
        // SAFETY: `fprog` points at the program, which outlives both calls.
        unsafe {
            assert_eq!(
                libc::prctl(
                    libc::PR_SET_NO_NEW_PRIVS,
                    1 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                ),
                0,
                "PR_SET_NO_NEW_PRIVS: {}",
                std::io::Error::last_os_error()
            );
            assert_eq!(
                libc::syscall(
                    libc::SYS_seccomp,
                    libc::SECCOMP_SET_MODE_FILTER as libc::c_long,
                    libc::SECCOMP_FILTER_FLAG_TSYNC as libc::c_long,
                    &fprog as *const libc::sock_fprog as libc::c_long,
                ),
                0,
                "seccomp(SECCOMP_SET_MODE_FILTER, TSYNC): {}",
                std::io::Error::last_os_error()
            );
        }
    }
}
