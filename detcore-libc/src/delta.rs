/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The libc constants, typedefs and structs that Detcore and detcore-model
//! name and reverie-syscalls' no-std `libc` module lacks.
//!
//! Guest memory is still Linux memory, so they must keep glibc's x86_64
//! values and layouts exactly. These definitions are libc 0.2.189's own
//! (x86_64-unknown-linux-gnu, as `rustc -Zunpretty=expanded` prints them),
//! copied verbatim by the generator of reverie-syscalls' module
//! (reverie-syscalls/shim-gen/gen_libc_shim.py), told to skip the names that
//! module already has.
//!
//! Each struct derives exactly the standard traits libc implements for it.
//!
//! Not yet checked by a test here: reverie-syscalls checks its module item by
//! item against `libc` on the host, but this crate compiles only for
//! `target_os = "none"`, where `libc` is empty.
#![allow(non_camel_case_types)]
#![allow(missing_docs)]

use core::mem::MaybeUninit;

use super::*;

/// libc's padding wrapper: uninitialized bytes that compare equal, hash to
/// nothing, and default to zero.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub(crate) struct Padding<T: Copy>(MaybeUninit<T>);

impl<T: Copy> Default for Padding<T> {
    fn default() -> Self {
        Self(MaybeUninit::zeroed())
    }
}

impl<T: Copy> core::fmt::Debug for Padding<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let full_name = core::any::type_name::<Self>();
        let prefix_len = full_name.find("Padding").unwrap();
        f.pad(&full_name[prefix_len..])
    }
}

impl<T: Copy> core::hash::Hash for Padding<T> {
    fn hash<H: core::hash::Hasher>(&self, _state: &mut H) {}
}

impl<T: Copy> PartialEq for Padding<T> {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl<T: Copy> Eq for Padding<T> {}

pub type __priority_which_t = c_uint;

pub type __rlimit_resource_t = c_uint;

pub type idtype_t = c_uint;

pub type in_addr_t = u32;

pub type in_port_t = u16;

pub const ADJ_FREQUENCY: c_uint = 0x0002;

pub const ADJ_OFFSET: c_uint = 0x0001;

pub const ADJ_OFFSET_SS_READ: c_uint = 0xa001;

pub const AF_INET: c_int = 2;

pub const AF_INET6: c_int = 10;

pub const AF_NETLINK: c_int = 16;

pub const AF_UNIX: c_int = 1;

pub const CLD_CONTINUED: c_int = 6;

pub const CLD_DUMPED: c_int = 3;

pub const CLD_EXITED: c_int = 1;

pub const CLD_KILLED: c_int = 2;

pub const CLD_STOPPED: c_int = 5;

pub const CLD_TRAPPED: c_int = 4;

pub const EAGAIN: c_int = 11;

pub const ECONNRESET: c_int = 104;

pub const EFD_CLOEXEC: c_int = 0x80000;

pub const EFD_NONBLOCK: c_int = 0x800;

pub const EINTR: c_int = 4;

pub const ENODEV: c_int = 19;

pub const ENOENT: c_int = 2;

pub const ENOSPC: c_int = 28;

pub const ETIMEDOUT: c_int = 110;

pub const EWOULDBLOCK: c_int = EAGAIN;

pub const FD_CLOEXEC: c_int = 0x1;

pub const FUTEX_CLOCK_REALTIME: c_int = 256;

pub const FUTEX_CMD_MASK: c_int = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

pub const FUTEX_FD: c_int = 2;

pub const FUTEX_PRIVATE_FLAG: c_int = 128;

pub const FUTEX_WAIT: c_int = 0;

pub const FUTEX_WAIT_BITSET: c_int = 9;

pub const FUTEX_WAKE: c_int = 1;

pub const FUTEX_WAKE_BITSET: c_int = 10;

pub const F_DUPFD_CLOEXEC: c_int = 1030;

pub const F_GETFD: c_int = 1;

pub const F_GETFL: c_int = 3;

pub const F_SETPIPE_SZ: c_int = 1031;

pub const GRND_INSECURE: c_uint = 0x0004;

pub const GRND_NONBLOCK: c_uint = 0x0001;

pub const GRND_RANDOM: c_uint = 0x0002;

pub const IN_CLOEXEC: c_int = O_CLOEXEC;

pub const IN_NONBLOCK: c_int = O_NONBLOCK;

pub const IPPROTO_TCP: c_int = 6;

pub const ITIMER_PROF: c_int = 2;

pub const ITIMER_REAL: c_int = 0;

pub const ITIMER_VIRTUAL: c_int = 1;

pub const LOCK_EX: c_int = 2;

pub const LOCK_NB: c_int = 4;

pub const LOCK_SH: c_int = 1;

pub const LOCK_UN: c_int = 8;

pub const MADV_COLD: c_int = 20;

pub const MADV_COLLAPSE: c_int = 25;

pub const MADV_DODUMP: c_int = 17;

pub const MADV_DOFORK: c_int = 11;

pub const MADV_DONTDUMP: c_int = 16;

pub const MADV_DONTFORK: c_int = 10;

pub const MADV_DONTNEED: c_int = 4;

pub const MADV_DONTNEED_LOCKED: c_int = 24;

pub const MADV_FREE: c_int = 8;

pub const MADV_HUGEPAGE: c_int = 14;

pub const MADV_HWPOISON: c_int = 100;

pub const MADV_KEEPONFORK: c_int = 19;

pub const MADV_MERGEABLE: c_int = 12;

pub const MADV_NOHUGEPAGE: c_int = 15;

pub const MADV_NORMAL: c_int = 0;

pub const MADV_PAGEOUT: c_int = 21;

pub const MADV_POPULATE_READ: c_int = 22;

pub const MADV_POPULATE_WRITE: c_int = 23;

pub const MADV_RANDOM: c_int = 1;

pub const MADV_REMOVE: c_int = 9;

pub const MADV_SEQUENTIAL: c_int = 2;

pub const MADV_SOFT_OFFLINE: c_int = 101;

pub const MADV_UNMERGEABLE: c_int = 13;

pub const MADV_WILLNEED: c_int = 3;

pub const MADV_WIPEONFORK: c_int = 18;

pub const MAP_ANONYMOUS: c_int = 0x0020;

pub const MAP_FAILED: *mut c_void = !0 as *mut c_void;

pub const MAP_PRIVATE: c_int = 0x0002;

pub const MAP_SHARED: c_int = 0x0001;

pub const MFD_CLOEXEC: c_uint = 0x0001;

pub const NETLINK_ROUTE: c_int = 0;

pub const NETLINK_SOCK_DIAG: c_int = 4;

pub const O_ACCMODE: c_int = 3;

pub const O_CLOEXEC: c_int = 0x80000;

pub const O_EXCL: c_int = 128;

pub const O_NONBLOCK: c_int = 2048;

pub const O_PATH: c_int = 0o10000000;

pub const O_RDWR: c_int = 2;

pub const PRIO_PGRP: __priority_which_t = 1;

pub const PRIO_PROCESS: __priority_which_t = 0;

pub const PRIO_USER: __priority_which_t = 2;

pub const PROT_READ: c_int = 1;

pub const PROT_WRITE: c_int = 2;

pub const PR_CAPBSET_DROP: c_int = 24;

pub const PR_CAPBSET_READ: c_int = 23;

pub const PR_CAP_AMBIENT: c_int = 47;

pub const PR_GET_DUMPABLE: c_int = 3;

pub const PR_GET_KEEPCAPS: c_int = 7;

pub const PR_GET_NAME: c_int = 16;

pub const PR_GET_PDEATHSIG: c_int = 2;

pub const PR_GET_THP_DISABLE: c_int = 42;

pub const PR_GET_TIMERSLACK: c_int = 30;

pub const PR_SET_DUMPABLE: c_int = 4;

pub const PR_SET_KEEPCAPS: c_int = 8;

pub const PR_SET_NAME: c_int = 15;

pub const PR_SET_NO_NEW_PRIVS: c_int = 38;

pub const PR_SET_PDEATHSIG: c_int = 1;

pub const PR_SET_SECUREBITS: c_int = 28;

pub const PR_SET_THP_DISABLE: c_int = 41;

pub const PR_SET_TIMERSLACK: c_int = 29;

pub const P_ALL: idtype_t = 0;

pub const P_PGID: idtype_t = 2;

pub const P_PID: idtype_t = 1;

pub const P_PIDFD: idtype_t = 3;

pub const RLIM64_INFINITY: rlim64_t = !0;

pub const RLIMIT_CORE: __rlimit_resource_t = 4;

pub const RLIMIT_CPU: __rlimit_resource_t = 0;

pub const RLIMIT_NOFILE: __rlimit_resource_t = 7;

pub const RLIMIT_RTTIME: __rlimit_resource_t = 15;

pub const RLIMIT_STACK: __rlimit_resource_t = 3;

pub const RUSAGE_CHILDREN: c_int = -1;

pub const RUSAGE_SELF: c_int = 0;

pub const RUSAGE_THREAD: c_int = 1;

pub const RWF_APPEND: c_int = 0x00000010;

pub const RWF_ATOMIC: c_int = 0x00000040;

pub const RWF_DONTCACHE: c_int = 0x00000080;

pub const RWF_DSYNC: c_int = 0x00000002;

pub const RWF_HIPRI: c_int = 0x00000001;

pub const RWF_NOAPPEND: c_int = 0x00000020;

pub const RWF_NOWAIT: c_int = 0x00000008;

pub const RWF_SYNC: c_int = 0x00000004;

pub const SA_RESTART: c_int = 0x10000000;

pub const SCHED_OTHER: c_int = 0;

pub const SCM_RIGHTS: c_int = 0x01;

pub const SFD_CLOEXEC: c_int = 0x080000;

pub const SFD_NONBLOCK: c_int = 0x0800;

pub const SIGCONT: c_int = 18;

pub const SIGEV_NONE: c_int = 1;

pub const SIGEV_SIGNAL: c_int = 0;

pub const SIGILL: c_int = 4;

pub const SIGINT: c_int = 2;

pub const SIGKILL: c_int = 9;

pub const SIGSEGV: c_int = 11;

pub const SIGSTKFLT: c_int = 16;

pub const SIGSTOP: c_int = 19;

pub const SIGTERM: c_int = 15;

pub const SIGTRAP: c_int = 5;

pub const SIGURG: c_int = 23;

pub const SIGWINCH: c_int = 28;

pub const SIG_BLOCK: c_int = 0x000000;

pub const SIG_DFL: sighandler_t = 0 as sighandler_t;

pub const SIG_IGN: sighandler_t = 1 as sighandler_t;

pub const SIG_SETMASK: c_int = 2;

pub const SI_KERNEL: c_int = 0x80;

pub const SOCK_CLOEXEC: c_int = O_CLOEXEC;

pub const SOCK_NONBLOCK: c_int = O_NONBLOCK;

pub const SOL_SOCKET: c_int = 1;

pub const SO_COOKIE: c_int = 57;

pub const SO_INCOMING_CPU: c_int = 49;

pub const SO_NETNS_COOKIE: c_int = 71;

pub const STA_UNSYNC: c_int = 0x0040;

pub const STDERR_FILENO: c_int = 2;

pub const STDIN_FILENO: c_int = 0;

pub const STDOUT_FILENO: c_int = 1;

pub const SYS_arch_prctl: c_long = 158;

pub const SYS_gettid: c_long = 186;

pub const SYS_pidfd_open: c_long = 434;

pub const SYS_pidfd_send_signal: c_long = 424;

pub const S_IFIFO: mode_t = 0o1_0000;

pub const S_IFMT: mode_t = 0o17_0000;

pub const S_IFREG: mode_t = 0o10_0000;

pub const S_IFSOCK: mode_t = 0o14_0000;

pub const TCP_INFO: c_int = 11;

pub const TFD_CLOEXEC: c_int = O_CLOEXEC;

pub const TFD_NONBLOCK: c_int = O_NONBLOCK;

pub const TIMER_ABSTIME: c_int = 1;

pub const TIME_ERROR: c_int = 5;

pub const UIO_MAXIOV: c_int = 1024;

pub const WCONTINUED: c_int = 0x00000008;

pub const WEXITED: c_int = 0x00000004;

pub const WNOHANG: c_int = 0x00000001;

pub const WNOWAIT: c_int = 0x01000000;

pub const WSTOPPED: c_int = WUNTRACED;

pub const WUNTRACED: c_int = 0x00000002;

pub const _SC_PAGESIZE: c_int = 30;

pub const __WALL: c_int = 0x40000000;

pub const __WCLONE: c_int = 0x80000000_u32 as c_int;

pub const __WNOTHREAD: c_int = 0x20000000;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct cmsghdr {
    pub cmsg_len: size_t,
    pub cmsg_level: c_int,
    pub cmsg_type: c_int,
}

#[repr(C)]
#[repr(align(4))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct in6_addr {
    pub s6_addr: [u8; 16],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct in_addr {
    pub s_addr: in_addr_t,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct sched_attr {
    pub size: __u32,
    pub sched_policy: __u32,
    pub sched_flags: __u64,
    pub sched_nice: __s32,
    pub sched_priority: __u32,
    pub sched_runtime: __u64,
    pub sched_deadline: __u64,
    pub sched_period: __u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct sockaddr_in {
    pub sin_family: sa_family_t,
    pub sin_port: in_port_t,
    pub sin_addr: in_addr,
    pub sin_zero: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct sockaddr_in6 {
    pub sin6_family: sa_family_t,
    pub sin6_port: in_port_t,
    pub sin6_flowinfo: u32,
    pub sin6_addr: in6_addr,
    pub sin6_scope_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct sockaddr_nl {
    pub nl_family: sa_family_t,
    nl_pad: Padding<c_ushort>,
    pub nl_pid: u32,
    pub nl_groups: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct sockaddr_un {
    pub sun_family: sa_family_t,
    pub sun_path: [c_char; 108],
}
