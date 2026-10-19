/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Which open file description (OFD) rows of `/proc/locks` are the guest's,
//! decided from the kernel's own state at the snapshot: the pure half.
//!
//! At a `/proc/locks` read, in the reader's turn, Detcore asks the kernel
//! which OFD locks the guest's live descriptor tables hold, through recorded
//! injected calls, and admits at most that many matching `OFDLCK ... -1`
//! rows. This module holds every decision of that procedure over the
//! kernel's answers, which its caller passes in as recorded results. It makes
//! no system call, so record and replay decide alike from the same answers.
//! Whether to scan, which tables to read and which questions to ask are
//! decided from guest state only, never from the snapshot's raw rows, which
//! include every host process's OFD locks.
//!
//! 1. [`scan_needed`]: only once the guest itself has locked some file
//!    ([`InodeSet`]); before that, no OFD row can be the guest's.
//! 2. [`TargetSelection`]: the tables to read. Every live guest process's (its
//!    leader's table, while the leader lives), and a live thread's only when
//!    `kcmp(KCMP_FILES)` says its table differs from every table already
//!    chosen for its process (`CLONE_THREAD` without `CLONE_FILES`,
//!    `unshare(CLONE_FILES)`, or a leader that exited while threads live).
//! 3. [`candidates`]: from each table's fdinfo, the descriptors whose
//!    description lists a granted `OFDLCK` line (Linux prints, under every
//!    descriptor of a description, exactly the locks that description owns).
//!    A lock counts only on an inode on which the guest itself took a lock
//!    through a description opened inside the container (the guest-locked
//!    [`InodeSet`]), and never on an inode a description inherited at launch
//!    or received from outside the container is open on (the foreign
//!    [`InodeSet`]), since a host process can lock through such a shared
//!    description at moments the host decides. A table or descriptor that
//!    could not be read contributes nothing and is named ([`Hidden`]).
//! 4. [`Multiplicity`]: the distinct guest descriptions holding L, told apart
//!    by `kcmp(KCMP_FILE)`, of whose answers only "equal" (0) is used, since 1
//!    and 2 order host pointers. Every candidate is classified (one comparison
//!    per candidate and class representative), whatever the snapshot's row
//!    count; m(L) is that class count capped at r(L), the snapshot's granted
//!    `OFDLCK -1` rows of L ([`raw_ofd_counts`]). A failed kcmp, or a reader
//!    outside the container's PID namespace (kcmp names tasks in the caller's
//!    namespace), caps m(L) at 1.
//! 5. [`keep_rows`]: a granted row stays when a guest process owns it by its
//!    positive pid (POSIX, `FLOCK` and lease rows are decided by owner only),
//!    when it is one of at most m(L) `OFDLCK -1` rows of L, or when its
//!    object is on device `00:00` (the temporary KVM exemption of
//!    [`super::locks`]). A waiter row stays only in a group that stays, when a
//!    guest process owns it, and never in a group of a lock L with
//!    r(L) > m(L): which of L's identical groups stays follows kernel list
//!    order, so a guest waiter behind it would appear at host-timed moments.
//!
//! In a PID namespace (every `--strict` run), every way this departs from
//! Linux hides a row; none shows a host's. A row is admitted against a lock
//! line read in this turn, for a description in a live guest table, and never
//! more of them than distinct such descriptions. Under `--no-namespace` one
//! host-row path remains (H2 of the design): a guest process Detcore still
//! lists whose pid the kernel has already freed and given to a host process,
//! whose table the scan would then read.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

use reverie::syscalls::Errno;
use serde::Deserialize;
use serde::Serialize;

use super::locks::HeldLock;
use super::locks::LockRowFacts;
use super::locks::held_locks_in_fdinfo;

/// The class `/proc/locks` and fdinfo print for an OFD lock.
const OFD_CLASS: &str = "OFDLCK";

/// One guest task, by the number the guest's `/proc` paths use and by the
/// number `kcmp` takes (`Scheduler::procfs_ids`; they differ on a backend
/// whose guest numbering is not the kernel's).
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
pub struct TaskIds {
    /// The guest's number for the task.
    pub guest: i32,
    /// The kernel's number for the task, in the container's PID namespace.
    pub kernel: i32,
}

/// A live guest process: its thread-group leader and its live non-leader
/// threads. A process that has exited is not listed, even while the kernel
/// is still tearing it down; leaving it out can only hide.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveProcess {
    /// The thread-group leader.
    pub leader: TaskIds,
    /// Whether the leader thread itself is live. A leader that exited while
    /// other threads live has no table (`/proc/<pid>/fdinfo` lists nothing),
    /// so one of the threads stands for the process.
    pub leader_live: bool,
    /// The live threads other than the leader.
    pub threads: Vec<TaskIds>,
}

/// A descriptor table the scan reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanTarget {
    /// A process's table, through `/proc/<pid>/fdinfo`.
    Process(TaskIds),
    /// A non-leader thread whose table the kernel says is not its leader's,
    /// through `/proc/<pid>/task/<tid>/fdinfo`.
    Thread {
        /// The thread's leader.
        leader: TaskIds,
        /// The thread.
        thread: TaskIds,
    },
}

impl ScanTarget {
    /// The task whose table this is, as `kcmp` names it.
    pub(crate) fn task(&self) -> TaskIds {
        match self {
            Self::Process(process) => *process,
            Self::Thread { thread, .. } => *thread,
        }
    }

    /// The table's directory, relative to the procfs anchor, by the kernel's
    /// numbers (the scan proves first that the anchor's procfs numbers tasks
    /// as the container's PID namespace does).
    fn task_directory(&self) -> String {
        match self {
            Self::Process(process) => format!("{}", process.kernel),
            Self::Thread { leader, thread } => format!("{}/task/{}", leader.kernel, thread.kernel),
        }
    }

    /// The table's `fd` directory, relative to the procfs anchor: its magic
    /// links name each descriptor's file, whose inode a `newfstatat` through
    /// them reads.
    pub(crate) fn fd_directory(&self) -> String {
        format!("{}/fd", self.task_directory())
    }

    /// Descriptor `fd`'s fdinfo entry, relative to the procfs anchor.
    pub(crate) fn fdinfo_entry(&self, fd: i32) -> String {
        format!("{}/fdinfo/{fd}", self.task_directory())
    }
}

impl fmt::Display for ScanTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Process(process) => write!(f, "process {}", process.guest),
            Self::Thread { leader, thread } => {
                write!(f, "thread {} of process {}", thread.guest, leader.guest)
            }
        }
    }
}

/// Why a table, a descriptor's fdinfo or a thread's table identity could not
/// be read. What it would have shown contributes no lock. Most reasons hide
/// ([`Self::hides`]); a failure caused by ambient, resource or host-policy
/// state leaves the snapshot undecidable, and the read refuses by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unreadable {
    /// A call failed (`ENOENT`, `EACCES`, `EMFILE`, ...).
    Failed {
        /// The call, as the INFO line names it.
        call: &'static str,
        /// Its error.
        errno: Errno,
    },
    /// The directory or entry is not procfs (`fstatfs`).
    NotProcfs,
    /// The directory or entry is not on the device of the procfs anchor and
    /// the reader's `/proc/locks` descriptor, so it is not that procfs
    /// instance.
    WrongDevice,
    /// The `fd` directory's listing did not parse as `getdents64` records.
    Unlisted,
    /// The reader is not in the container's PID namespace, so `kcmp` would
    /// name other tasks.
    NestedPidNamespace,
    /// The procfs does not number the reader as the container's PID
    /// namespace does (`<anchor>/<pid>` is not `<anchor>/self`): it belongs
    /// to another PID namespace, whose numbers name other tasks.
    NotTheContainersProcfs,
    /// The lock's file is also open through a description inherited at
    /// launch or received from outside the container, which a host process
    /// can lock through.
    SharedOutside,
}

/// Whether a scan step that failed with `errno` hides what it would have
/// read: a cause the guest's own filesystem view or the backend decides, the
/// same in every run of the same program (no `/proc` or an entry missing,
/// ENOENT or ENOTDIR; a symlink loop, ELOOP; an entry outside the anchor or
/// across a mount, EXDEV; a backend that refuses the call, EINVAL, as KVM
/// refuses an `O_PATH` open of its `/proc`). Every other errno is ambient,
/// resource or host-policy state that can differ between runs or hosts, so
/// hiding on it would let host state decide whether a guest row shows, and
/// the read refuses instead: a full descriptor table, EMFILE; a full host
/// file table, ENFILE; no memory, ENOMEM; an interruption, EINTR; an I/O
/// error, EIO; EAGAIN; a kernel without `openat2` or `kcmp`, ENOSYS; a
/// permission refusal, EACCES or EPERM, which a host security module keyed by
/// the host process can give as well as the guest's own seccomp filter or a
/// target another uid owns or that is not dumpable; anything unlisted. This
/// is the classification of the procfs-anchored fdinfo read it replaces.
pub(crate) fn errno_hides(errno: Errno) -> bool {
    matches!(
        errno,
        Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP | Errno::EXDEV | Errno::EINVAL
    )
}

impl Unreadable {
    /// Whether this hides (see [`errno_hides`]); otherwise the snapshot
    /// cannot be decided.
    pub(crate) fn hides(&self) -> bool {
        match self {
            Self::Failed { errno, .. } => errno_hides(*errno),
            Self::NotProcfs
            | Self::WrongDevice
            | Self::Unlisted
            | Self::NestedPidNamespace
            | Self::SharedOutside => true,
            Self::NotTheContainersProcfs => false,
        }
    }
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed { call, errno } => write!(f, "{call} failed with {errno:?}"),
            Self::NotProcfs => f.write_str("it is not procfs"),
            Self::WrongDevice => f.write_str("it is not on the /proc/locks procfs device"),
            Self::Unlisted => f.write_str("its listing did not parse"),
            Self::NestedPidNamespace => {
                f.write_str("the reader is not in the container's PID namespace")
            }
            Self::NotTheContainersProcfs => f.write_str(
                "the procfs does not number the reader as the container's PID namespace does",
            ),
            Self::SharedOutside => f.write_str(
                "its file is also open through a description inherited at launch or received \
                 from outside the container",
            ),
        }
    }
}

/// Something the scan could not read, and so left out. Its `Display` is the
/// INFO line that names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hidden {
    /// The table.
    pub(crate) target: ScanTarget,
    /// The descriptor, or `None` for the table as a whole.
    pub(crate) fd: Option<i32>,
    /// Why.
    pub(crate) reason: Unreadable,
}

impl fmt::Display for Hidden {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.fd {
            Some(fd) => write!(
                f,
                "/proc/locks: the OFD locks of descriptor {fd} of {} are hidden: {}",
                self.target, self.reason
            ),
            None => write!(
                f,
                "/proc/locks: the OFD locks of the descriptor table of {} are hidden: {}",
                self.target, self.reason
            ),
        }
    }
}

/// Whether `row` is a granted OFD lock as `/proc/locks` prints it from any
/// namespace (owner `-1`), on a real device.
fn is_ofd_row(row: &LockRowFacts) -> bool {
    !row.waiter && row.owner == -1 && row.lock.class == OFD_CLASS && !row.lock.on_null_device()
}

/// r(L): how many granted `OFDLCK` rows with owner `-1` each lock L has in
/// the snapshot. Rows on device `00:00` are left to the KVM exemption.
pub(crate) fn raw_ofd_counts(rows: &[LockRowFacts]) -> BTreeMap<HeldLock, usize> {
    let mut counts = BTreeMap::new();
    for row in rows.iter().filter(|row| is_ofd_row(row)) {
        *counts.entry(row.lock.clone()).or_default() += 1;
    }
    counts
}

/// Whether a `/proc/locks` read scans: only once the guest itself has
/// locked some file. Decided from guest state only, so whether a scan runs,
/// and with it its refusals, INFO lines and kcmp calls, never depends on host
/// processes' OFD locks in the snapshot. Without a guest-locked inode no
/// candidate can exist ([`candidates`]), so no OFD row is the guest's.
pub(crate) fn scan_needed(locked: &InodeSet) -> bool {
    !locked.is_empty()
}

/// The tables to read, chosen by `kcmp(KCMP_FILES)` one question at a time
/// so that its caller can inject each call and pass back the recorded
/// answer.
///
/// For each live process, its leader's table while the leader lives; then
/// each live thread, compared with one representative of every table already
/// chosen for that process until one is equal (0: the same table) or all
/// differ (a new table to read, and a new representative). A process whose
/// leader exited is read through its first live thread. A failed kcmp leaves
/// the thread unread, named. A reader outside the container's PID namespace
/// asks no kcmp: threads after a process's first table are left unread.
/// Processes that share one table (`CLONE_FILES` without `CLONE_THREAD`) are
/// each read; [`Multiplicity`] counts their shared descriptions once.
#[derive(Clone, Debug)]
pub(crate) struct TargetSelection {
    processes: Vec<LiveProcess>,
    kcmp_usable: bool,
    targets: Vec<ScanTarget>,
    hidden: Vec<Hidden>,
    /// The process and the thread being placed, and the representative it is
    /// compared with next.
    process: usize,
    thread: usize,
    representative: usize,
    representatives: Vec<TaskIds>,
}

impl TargetSelection {
    /// The selection for `processes`; `kcmp_usable` is false for a reader
    /// outside the container's PID namespace.
    pub(crate) fn new(processes: Vec<LiveProcess>, kcmp_usable: bool) -> Self {
        let mut selection = Self {
            processes,
            kcmp_usable,
            targets: Vec::new(),
            hidden: Vec::new(),
            process: 0,
            thread: 0,
            representative: 0,
            representatives: Vec::new(),
        };
        selection.start_process();
        selection.settle();
        selection
    }

    fn start_process(&mut self) {
        self.thread = 0;
        self.representative = 0;
        self.representatives.clear();
        if let Some(process) = self.processes.get(self.process)
            && process.leader_live
        {
            self.targets.push(ScanTarget::Process(process.leader));
            self.representatives.push(process.leader);
        }
    }

    /// Place every thread that needs no question, stopping at the first that
    /// does, or at the end.
    fn settle(&mut self) {
        while let Some(process) = self.processes.get(self.process) {
            let Some(thread) = process.threads.get(self.thread).copied() else {
                self.process += 1;
                self.start_process();
                continue;
            };
            let leader = process.leader;
            if self.representatives.is_empty() {
                self.targets.push(ScanTarget::Thread { leader, thread });
                self.representatives.push(thread);
                self.thread += 1;
            } else if !self.kcmp_usable {
                self.hidden.push(Hidden {
                    target: ScanTarget::Thread { leader, thread },
                    fd: None,
                    reason: Unreadable::NestedPidNamespace,
                });
                self.thread += 1;
            } else {
                return;
            }
        }
    }

    /// The next `kcmp(KCMP_FILES, representative, thread)` to ask, or `None`
    /// when every table is chosen.
    pub(crate) fn question(&self) -> Option<(TaskIds, TaskIds)> {
        let process = self.processes.get(self.process)?;
        let thread = *process.threads.get(self.thread)?;
        Some((self.representatives[self.representative], thread))
    }

    /// The recorded answer to [`Self::question`], or why it has none.
    pub(crate) fn answer(&mut self, answer: Result<i64, Unreadable>) {
        let Some(process) = self.processes.get(self.process) else {
            return;
        };
        let Some(thread) = process.threads.get(self.thread).copied() else {
            return;
        };
        let leader = process.leader;
        match answer {
            Ok(0) => self.next_thread(),
            Ok(_) if self.representative + 1 < self.representatives.len() => {
                self.representative += 1;
            }
            Ok(_) => {
                self.targets.push(ScanTarget::Thread { leader, thread });
                self.representatives.push(thread);
                self.next_thread();
            }
            Err(reason) => {
                self.hidden.push(Hidden {
                    target: ScanTarget::Thread { leader, thread },
                    fd: None,
                    reason,
                });
                self.next_thread();
            }
        }
    }

    fn next_thread(&mut self) {
        self.thread += 1;
        self.representative = 0;
        self.settle();
    }

    /// The tables to read and the threads left unread, once
    /// [`Self::question`] is `None`.
    pub(crate) fn finish(self) -> (Vec<ScanTarget>, Vec<Hidden>) {
        (self.targets, self.hidden)
    }
}

/// Whether the procfs the scan reads numbers tasks as the container's PID
/// namespace does, from `(st_dev, st_ino)` of `<anchor>/<the reader's pid>`
/// and of `<anchor>/self`, or the failed call and its error: they must be one
/// directory. A procfs of another PID namespace names another task, or none,
/// by the reader's number. Every failure refuses: a missing directory
/// (ENOENT, and every other errno that would hide elsewhere) proves the
/// numbering differs, and any other errno is ambient, resource or host-policy
/// state.
pub(crate) fn procfs_numbers_the_reader(
    by_number: Result<(u64, u64), (&'static str, Errno)>,
    by_self: Result<(u64, u64), (&'static str, Errno)>,
) -> Result<(), Unreadable> {
    match (by_number, by_self) {
        (Ok(number), Ok(own)) if number == own => Ok(()),
        (Ok(_), Ok(_)) => Err(Unreadable::NotTheContainersProcfs),
        (Err((call, errno)), _) | (_, Err((call, errno))) => {
            if errno_hides(errno) {
                Err(Unreadable::NotTheContainersProcfs)
            } else {
                Err(Unreadable::Failed { call, errno })
            }
        }
    }
}

/// A set of inode numbers that only grows, keyed by inode number alone, on
/// any device, because a file in another btrfs subvolume prints the
/// superblock's device in `/proc/locks` and fdinfo but another in `fstat`.
///
/// Two are kept. The guest-locked set: the inodes on which the guest itself
/// took a lock (a successful `F_OFD_SETLK` or `F_OFD_SETLKW` other than
/// `F_UNLCK`, or `flock` other than `LOCK_UN`) through a description opened
/// inside the container. A lock counts only on one of them, so a description
/// the guest shares with a host process does not show locks only the host
/// takes through it. The foreign set: the inodes a description inherited at
/// launch or received from outside the container is open on. A lock never
/// counts on one of them, since the host can lock through that description at
/// moments it decides. A description the guest shares with the host can still
/// change between the raw capture and the scan (H1); with both sets, only on
/// an inode the guest locked through its own description and no foreign
/// description is open on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InodeSet(BTreeSet<u64>);

impl InodeSet {
    /// Add `inode`.
    pub(crate) fn note(&mut self, inode: u64) {
        self.0.insert(inode);
    }

    /// Whether the set is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `inode` is in the set.
    pub(crate) fn has(&self, inode: u64) -> bool {
        self.0.contains(&inode)
    }

    /// Whether `lock`'s object is on an inode in the set.
    pub(crate) fn admits(&self, lock: &HeldLock) -> bool {
        lock.object
            .rsplit(':')
            .next()
            .and_then(|inode| inode.parse::<u64>().ok())
            .is_some_and(|inode| self.0.contains(&inode))
    }
}

/// The descriptor numbers a batch of `getdents64` records of an `fd` or
/// fdinfo directory names, in record order: each `struct linux_dirent64` is
/// `d_ino` (8 bytes), `d_off` (8), `d_reclen` (2), `d_type` (1) and a
/// NUL-terminated name, `d_reclen` bytes in all. Names that are not a
/// descriptor number (`.`, `..`) are skipped. `None` when a record runs past
/// the batch or is shorter than its header.
pub(crate) fn fdinfo_directory_fds(batch: &[u8]) -> Option<Vec<i32>> {
    const NAME: usize = 19;
    let mut fds = Vec::new();
    let mut rest = batch;
    while !rest.is_empty() {
        let length = usize::from(u16::from_ne_bytes(rest.get(16..18)?.try_into().ok()?));
        if length < NAME || length > rest.len() {
            return None;
        }
        let name = &rest[NAME..length];
        let name = &name[..name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name.len())];
        if let Some(fd) = std::str::from_utf8(name)
            .ok()
            .filter(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|name| name.parse::<i32>().ok())
        {
            fds.push(fd);
        }
        rest = &rest[length..];
    }
    Some(fds)
}

/// What reading one table returned, as recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TableScan {
    /// The `fd` directory could not be opened, checked or listed.
    Unreadable(Unreadable),
    /// Each descriptor whose file is on a guest-locked inode, with its fdinfo
    /// text or why it could not be read. The scan's own descriptors in the
    /// reader's table, and descriptors on other inodes, are not listed.
    Read(Vec<(i32, Result<Vec<u8>, Unreadable>)>),
}

/// A descriptor whose description holds some lock: `kcmp(KCMP_FILE)` names
/// it by its task and number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// The task whose table holds the descriptor.
    pub(crate) task: TaskIds,
    /// The descriptor.
    pub(crate) fd: i32,
}

/// The descriptors whose description holds each lock, and what the scan left
/// out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Candidates {
    /// For each granted OFD lock some guest description holds, every
    /// descriptor that lists it, in scan order.
    pub(crate) by_lock: BTreeMap<HeldLock, Vec<Candidate>>,
    /// What could not be read, in scan order.
    pub(crate) hidden: Vec<Hidden>,
}

/// The descriptors whose fdinfo lists each granted `OFDLCK` lock on an inode
/// the guest locked (`gate`) and no foreign description is open on
/// (`foreign`; such a lock is left out and named). `FLOCK`, POSIX and waiter
/// lines are left out: flock rows are decided by their owner, POSIX rows are
/// a task's, and fdinfo lists no waiters.
pub(crate) fn candidates(
    scans: &[(ScanTarget, TableScan)],
    gate: &InodeSet,
    foreign: &InodeSet,
) -> Candidates {
    let mut found = Candidates::default();
    for (target, scan) in scans {
        let descriptors = match scan {
            TableScan::Unreadable(reason) => {
                found.hidden.push(Hidden {
                    target: *target,
                    fd: None,
                    reason: *reason,
                });
                continue;
            }
            TableScan::Read(descriptors) => descriptors,
        };
        for (fd, fdinfo) in descriptors {
            let fdinfo = match fdinfo {
                Ok(fdinfo) => fdinfo,
                Err(reason) => {
                    found.hidden.push(Hidden {
                        target: *target,
                        fd: Some(*fd),
                        reason: *reason,
                    });
                    continue;
                }
            };
            for lock in held_locks_in_fdinfo(fdinfo) {
                if lock.class != OFD_CLASS || !gate.admits(&lock) {
                    continue;
                }
                if foreign.admits(&lock) {
                    let hidden = Hidden {
                        target: *target,
                        fd: Some(*fd),
                        reason: Unreadable::SharedOutside,
                    };
                    if !found.hidden.contains(&hidden) {
                        found.hidden.push(hidden);
                    }
                    continue;
                }
                found.by_lock.entry(lock).or_default().push(Candidate {
                    task: target.task(),
                    fd: *fd,
                });
            }
        }
    }
    found
}

/// How many distinct guest descriptions hold one lock L, asked of
/// `kcmp(KCMP_FILE)` one question at a time so that its caller can inject
/// each call and pass back the recorded answer. m(L) is this count capped at
/// the snapshot's row count r(L) ([`Self::count`]); the questions do not
/// depend on r(L), which counts host processes' rows too.
///
/// Each candidate is compared with one representative of every class found
/// so far, until it equals one (same description) or differs from all (a new
/// class): at most one comparison per candidate and representative. Only 0
/// is read as "equal"; 1, 2 and any other answer mean two descriptions.
#[derive(Clone, Debug)]
pub(crate) struct Multiplicity {
    candidates: Vec<Candidate>,
    /// Indexes into `candidates` of one representative per class.
    representatives: Vec<usize>,
    /// The candidate being classified, and the representative it is compared
    /// with next.
    next: Option<(usize, usize)>,
    /// The class count when no question remains.
    settled: Option<usize>,
}

impl Multiplicity {
    /// The classes of these candidates. `kcmp_usable` is false when the
    /// reader is not in the container's PID namespace, where kcmp would name
    /// other tasks: one class is then all that is certain.
    pub(crate) fn new(candidates: Vec<Candidate>, kcmp_usable: bool) -> Self {
        let settled = match candidates.len() {
            0 => Some(0),
            1 => Some(1),
            _ if !kcmp_usable => Some(1),
            _ => None,
        };
        Self {
            next: settled.is_none().then_some((1, 0)),
            representatives: vec![0],
            candidates,
            settled,
        }
    }

    /// The next comparison to make: `kcmp(KCMP_FILE)` of the first
    /// descriptor with the second, or `None` when the classes are decided.
    pub(crate) fn question(&self) -> Option<(Candidate, Candidate)> {
        let (candidate, representative) = self.next?;
        Some((
            self.candidates[candidate],
            self.candidates[self.representatives[representative]],
        ))
    }

    /// The recorded answer to [`Self::question`]. A failed kcmp settles one
    /// class: which candidates share a description is then unknown, and one
    /// is certain. The caller refuses the read instead when the failure is not
    /// one that hides ([`errno_hides`]).
    pub(crate) fn answer(&mut self, answer: Result<i64, Errno>) {
        let Some((candidate, representative)) = self.next else {
            return;
        };
        let same = match answer {
            Ok(order) => order == 0,
            Err(_) => {
                self.finish(1);
                return;
            }
        };
        if !same && representative + 1 < self.representatives.len() {
            self.next = Some((candidate, representative + 1));
            return;
        }
        if !same {
            self.representatives.push(candidate);
        }
        if candidate + 1 < self.candidates.len() {
            self.next = Some((candidate + 1, 0));
        } else {
            self.finish(self.representatives.len());
        }
    }

    fn finish(&mut self, count: usize) {
        self.next = None;
        self.settled = Some(count);
    }

    /// m(L): the class count capped at `raw`, the snapshot's granted
    /// `OFDLCK -1` rows of L, once [`Self::question`] is `None`.
    pub(crate) fn count(&self, raw: usize) -> Option<usize> {
        self.settled.map(|classes| classes.min(raw))
    }
}

/// Which of `rows` stay (see the module comment). `multiplicity` holds m(L)
/// for each OFD lock L the scan found guest descriptions holding; a lock it
/// does not name has m(L) = 0, so an empty map (no scan, or a scan that must
/// not be trusted) hides every OFD row. `guest_process` answers for a
/// positive owner.
pub(crate) fn keep_rows(
    rows: &[LockRowFacts],
    mut guest_process: impl FnMut(i64) -> bool,
    multiplicity: &BTreeMap<HeldLock, usize>,
) -> Vec<bool> {
    let raw = raw_ofd_counts(rows);
    let mut budget = raw
        .iter()
        .map(|(lock, rows)| {
            let held = multiplicity.get(lock).copied().unwrap_or(0);
            (lock, held.min(*rows))
        })
        .collect::<BTreeMap<_, _>>();
    let ambiguous = rows
        .iter()
        .filter(|row| is_ofd_row(row) && raw[&row.lock] > budget[&row.lock])
        .map(|row| row.sequence.as_str())
        .collect::<BTreeSet<_>>();
    let mut kept_groups = BTreeSet::new();
    let mut keep = vec![false; rows.len()];
    for (index, row) in rows.iter().enumerate() {
        if row.waiter {
            continue;
        }
        keep[index] = row.lock.on_null_device()
            || (row.owner > 0 && guest_process(row.owner))
            || (is_ofd_row(row)
                && budget.get_mut(&row.lock).is_some_and(|left| {
                    let available = *left > 0;
                    *left = left.saturating_sub(1);
                    available
                }));
        if keep[index] {
            kept_groups.insert(row.sequence.as_str());
        }
    }
    for (index, row) in rows.iter().enumerate() {
        if row.waiter {
            keep[index] = kept_groups.contains(row.sequence.as_str())
                && !ambiguous.contains(row.sequence.as_str())
                && (row.lock.on_null_device() || (row.owner > 0 && guest_process(row.owner)));
        }
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::super::locks::lock_rows;
    use super::*;

    fn task(guest: i32) -> TaskIds {
        TaskIds {
            guest,
            kernel: guest + 1000,
        }
    }

    fn lock(object: &str, start: &str, end: &str) -> HeldLock {
        HeldLock {
            class: OFD_CLASS.to_owned(),
            mode: "ADVISORY".to_owned(),
            access: "WRITE".to_owned(),
            object: object.to_owned(),
            start: start.to_owned(),
            end: end.to_owned(),
        }
    }

    fn gate(inodes: &[u64]) -> InodeSet {
        let mut gate = InodeSet::default();
        for inode in inodes {
            gate.note(*inode);
        }
        gate
    }

    fn candidate(guest: i32, fd: i32) -> Candidate {
        Candidate {
            task: task(guest),
            fd,
        }
    }

    // A description's fdinfo: one granted OFD lock on 1002, a POSIX lock and
    // a flock the kernel lists beside it, a waiter line (which Linux does not
    // print in fdinfo, and which is not counted if it were) and a line that is
    // not a lock.
    const FDINFO: &[u8] = b"pos:\t0\nflags:\t02100002\nmnt_id:\t31\nino:\t1002\n\
lock:\t1: OFDLCK ADVISORY  WRITE -1 00:2a:1002 0 0\n\
lock:\t2: POSIX  ADVISORY  WRITE 3 00:2a:1002 5 5\n\
lock:\t3: FLOCK  ADVISORY  WRITE 3 00:2a:1002 0 EOF\n\
lock:\t4: -> OFDLCK ADVISORY  WRITE -1 00:2a:1002 7 7\n\
lock:\tnot a lock\n";

    #[test]
    fn fdinfo_contributes_only_its_granted_ofd_locks() {
        let scans = [(
            ScanTarget::Process(task(3)),
            TableScan::Read(vec![(4, Ok(FDINFO.to_vec()))]),
        )];
        let found = candidates(&scans, &gate(&[1002]), &InodeSet::default());
        assert!(found.hidden.is_empty());
        assert_eq!(
            found.by_lock.into_iter().collect::<Vec<_>>(),
            [(lock("00:2a:1002", "0", "0"), vec![candidate(3, 4)])]
        );
    }

    fn dirent(name: &str) -> Vec<u8> {
        let length = (19 + name.len() + 1).next_multiple_of(8);
        let mut record = vec![0u8; length];
        record[16..18].copy_from_slice(&(length as u16).to_ne_bytes());
        record[18] = 8;
        record[19..19 + name.len()].copy_from_slice(name.as_bytes());
        record
    }

    /// A failure the guest's filesystem view or the backend decides hides;
    /// ambient, resource or host-policy state, and anything unlisted, leaves the snapshot
    /// undecidable, so the read refuses.
    #[test]
    fn only_a_guest_or_backend_decided_failure_hides() {
        let failed = |errno| Unreadable::Failed {
            call: "openat2",
            errno,
        };
        for errno in [
            Errno::ENOENT,
            Errno::ENOTDIR,
            Errno::ELOOP,
            Errno::EXDEV,
            Errno::EINVAL,
        ] {
            assert!(failed(errno).hides(), "{errno}");
        }
        for errno in [
            Errno::EACCES,
            Errno::EPERM,
            Errno::EMFILE,
            Errno::ENFILE,
            Errno::ENOMEM,
            Errno::EINTR,
            Errno::EIO,
            Errno::EAGAIN,
            Errno::ENOSYS,
            Errno::EBADF,
        ] {
            assert!(!failed(errno).hides(), "{errno}");
        }
        for reason in [
            Unreadable::NotProcfs,
            Unreadable::WrongDevice,
            Unreadable::Unlisted,
            Unreadable::NestedPidNamespace,
            Unreadable::SharedOutside,
        ] {
            assert!(reason.hides(), "{reason}");
        }
        // A procfs of another PID namespace names other tasks by the
        // container's numbers: refused, never hidden.
        assert!(!Unreadable::NotTheContainersProcfs.hides());
    }

    /// A lock on an inode a description inherited at launch or received
    /// from outside the container is open on is left out and named, even
    /// though the guest locked that inode through its own description.
    #[test]
    fn a_lock_on_a_foreign_inode_is_hidden_and_named() {
        let fdinfo = b"lock:\t1: OFDLCK ADVISORY  READ  -1 00:2a:1002 5 5\n\
lock:\t2: OFDLCK ADVISORY  READ  -1 00:2a:1003 0 0\n"
            .to_vec();
        let scans = [(
            ScanTarget::Process(task(3)),
            TableScan::Read(vec![(0, Ok(fdinfo))]),
        )];
        let found = candidates(&scans, &gate(&[1002, 1003]), &gate(&[1002]));
        assert_eq!(
            found
                .by_lock
                .into_keys()
                .map(|lock| lock.object)
                .collect::<Vec<_>>(),
            ["00:2a:1003"]
        );
        assert_eq!(
            found
                .hidden
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "/proc/locks: the OFD locks of descriptor 0 of process 3 are hidden: its file is \
                 also open through a description inherited at launch or received from outside \
                 the container"
            ]
        );
    }

    /// A lock on another device with the same inode number is another lock:
    /// the gate is keyed by inode alone, but admission needs the lock itself.
    #[test]
    fn a_lock_on_another_device_is_not_admitted_by_the_inode_gate() {
        let rows = lock_rows(
            b"1: OFDLCK ADVISORY WRITE -1 00:18:7 0 EOF\n2: OFDLCK ADVISORY WRITE -1 00:2a:7 0 EOF\n",
        )
        .unwrap();
        let scans = [(
            ScanTarget::Process(task(3)),
            TableScan::Read(vec![(
                4,
                Ok(b"lock:\t1: OFDLCK ADVISORY WRITE -1 00:18:7 0 EOF\n".to_vec()),
            )]),
        )];
        let found = candidates(&scans, &gate(&[7]), &InodeSet::default());
        let multiplicity = found
            .by_lock
            .into_iter()
            .map(|(lock, candidates)| (lock, candidates.len()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(keep_rows(&rows, guest, &multiplicity), [true, false]);
    }

    /// The scan reads only a procfs that names the reader by the container's
    /// number: `<anchor>/<pid>` and `<anchor>/self` one directory. A procfs of
    /// another PID namespace (an ancestor one, whose `<pid>` is another task
    /// or none) refuses, never hides. Hermit's own runs cannot build one: the
    /// guest cannot mount (Detcore refuses mount), and a `--bind` of `/proc`
    /// binds the container's own procfs, so this is decided here from fake
    /// answers.
    #[test]
    fn only_a_procfs_numbering_the_reader_as_the_container_does_is_read() {
        let directory = (0x16, 4242);
        assert_eq!(
            procfs_numbers_the_reader(Ok(directory), Ok(directory)),
            Ok(())
        );
        // An ancestor namespace's procfs: the reader's container number names
        // another task's directory.
        assert_eq!(
            procfs_numbers_the_reader(Ok((0x16, 3)), Ok(directory)),
            Err(Unreadable::NotTheContainersProcfs)
        );
        // Or no task at all.
        for errno in [Errno::ENOENT, Errno::ENOTDIR, Errno::EXDEV] {
            let refused = procfs_numbers_the_reader(Err(("openat2", errno)), Ok(directory));
            assert_eq!(refused, Err(Unreadable::NotTheContainersProcfs));
            assert!(!refused.unwrap_err().hides());
        }
        // Resource or host-policy state refuses by its own name.
        let refused = procfs_numbers_the_reader(Ok(directory), Err(("fstat", Errno::EMFILE)));
        assert_eq!(
            refused,
            Err(Unreadable::Failed {
                call: "fstat",
                errno: Errno::EMFILE
            })
        );
        assert!(!refused.unwrap_err().hides());
    }

    #[test]
    fn a_directory_listing_names_its_descriptors() {
        let batch = [
            dirent("."),
            dirent(".."),
            dirent("0"),
            dirent("17"),
            dirent("x"),
        ]
        .concat();
        assert_eq!(fdinfo_directory_fds(&batch), Some(vec![0, 17]));
        assert_eq!(fdinfo_directory_fds(&[]), Some(vec![]));
        let mut truncated = dirent("3");
        truncated.pop();
        assert_eq!(
            fdinfo_directory_fds(&[dirent("1"), truncated].concat()),
            None
        );
        assert_eq!(fdinfo_directory_fds(&[0u8; 10]), None);
    }

    /// Runs a selection to the end, answering `kcmp(KCMP_FILES)` from
    /// `table`, which names each task's table (`None`: the call fails).
    fn select(
        processes: Vec<LiveProcess>,
        kcmp_usable: bool,
        table: impl Fn(i32) -> Option<char>,
    ) -> (Vec<ScanTarget>, Vec<Hidden>, Vec<(i32, i32)>) {
        let mut selection = TargetSelection::new(processes, kcmp_usable);
        let mut asked = Vec::new();
        while let Some((representative, thread)) = selection.question() {
            asked.push((representative.kernel, thread.kernel));
            selection.answer(match (table(representative.guest), table(thread.guest)) {
                (Some(left), Some(right)) => Ok(i64::from(left != right)),
                _ => Err(Unreadable::Failed {
                    call: "kcmp(KCMP_FILES)",
                    errno: Errno::EPERM,
                }),
            });
        }
        let (targets, hidden) = selection.finish();
        (targets, hidden, asked)
    }

    fn live(leader: i32, leader_live: bool, threads: &[i32]) -> LiveProcess {
        LiveProcess {
            leader: task(leader),
            leader_live,
            threads: threads.iter().map(|thread| task(*thread)).collect(),
        }
    }

    #[test]
    fn a_thread_is_scanned_only_when_its_table_is_new_to_its_process() {
        // Process 3: thread 4 shares the leader's table A; 5 and 6 share B;
        // 9 has C; asking about 7 fails. Process 8's leader exited; its
        // threads 10 and 11 share D. Process 12 has no threads.
        let table = |task: i32| match task {
            3 | 4 => Some('A'),
            5 | 6 => Some('B'),
            9 => Some('C'),
            10 | 11 => Some('D'),
            _ => None,
        };
        let processes = vec![
            live(3, true, &[4, 5, 6, 7, 9]),
            live(8, false, &[10, 11]),
            live(12, true, &[]),
        ];
        let (targets, hidden, asked) = select(processes.clone(), true, table);
        assert_eq!(
            asked,
            [
                (1003, 1004),
                (1003, 1005),
                (1003, 1006),
                (1005, 1006),
                (1003, 1007),
                (1003, 1009),
                (1005, 1009),
                (1010, 1011),
            ]
        );
        let thread = |leader, thread| ScanTarget::Thread {
            leader: task(leader),
            thread: task(thread),
        };
        assert_eq!(
            targets,
            [
                ScanTarget::Process(task(3)),
                thread(3, 5),
                thread(3, 9),
                thread(8, 10),
                ScanTarget::Process(task(12)),
            ]
        );
        assert_eq!(
            targets
                .iter()
                .map(ScanTarget::fd_directory)
                .collect::<Vec<_>>(),
            [
                "1003/fd",
                "1003/task/1005/fd",
                "1003/task/1009/fd",
                "1008/task/1010/fd",
                "1012/fd",
            ]
        );
        assert_eq!(targets[1].fdinfo_entry(7), "1003/task/1005/fdinfo/7");
        assert_eq!(targets[0].fdinfo_entry(0), "1003/fdinfo/0");
        assert_eq!(
            hidden.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "/proc/locks: the OFD locks of the descriptor table of thread 7 of process 3 \
                 are hidden: kcmp(KCMP_FILES) failed with EPERM"
            ]
        );
        assert_eq!(targets[1].task().kernel, 1005);
        // Outside the container's PID namespace no kcmp is asked: a
        // process's first table is read, and its other threads are not.
        let (targets, hidden, asked) = select(processes, false, table);
        assert!(asked.is_empty());
        assert_eq!(
            targets,
            [
                ScanTarget::Process(task(3)),
                thread(8, 10),
                ScanTarget::Process(task(12)),
            ]
        );
        assert_eq!(hidden.len(), 6);
        assert!(
            hidden
                .iter()
                .all(|hidden| hidden.reason == Unreadable::NestedPidNamespace)
        );
    }

    #[test]
    fn what_cannot_be_read_contributes_nothing_and_is_named() {
        let held = b"lock:\t1: OFDLCK ADVISORY  WRITE -1 00:2a:1002 0 0\n".to_vec();
        let thread = ScanTarget::Thread {
            leader: task(3),
            thread: task(5),
        };
        let scans = [
            (
                ScanTarget::Process(task(3)),
                TableScan::Read(vec![
                    (
                        4,
                        Err(Unreadable::Failed {
                            call: "openat2",
                            errno: Errno::EMFILE,
                        }),
                    ),
                    (5, Ok(held)),
                    (6, Err(Unreadable::WrongDevice)),
                ]),
            ),
            (
                ScanTarget::Process(task(8)),
                TableScan::Unreadable(Unreadable::Failed {
                    call: "openat",
                    errno: Errno::EACCES,
                }),
            ),
            (thread, TableScan::Unreadable(Unreadable::WrongDevice)),
        ];
        let found = candidates(&scans, &gate(&[1002]), &InodeSet::default());
        assert_eq!(
            found.by_lock.into_iter().collect::<Vec<_>>(),
            [(lock("00:2a:1002", "0", "0"), vec![candidate(3, 5)])]
        );
        assert_eq!(
            found
                .hidden
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "/proc/locks: the OFD locks of descriptor 4 of process 3 are hidden: openat2 \
                 failed with EMFILE",
                "/proc/locks: the OFD locks of descriptor 6 of process 3 are hidden: it is not \
                 on the /proc/locks procfs device",
                "/proc/locks: the OFD locks of the descriptor table of process 8 are hidden: \
                 openat failed with EACCES",
                "/proc/locks: the OFD locks of the descriptor table of thread 5 of process 3 \
                 are hidden: it is not on the /proc/locks procfs device",
            ]
        );
    }

    #[test]
    fn a_lock_counts_only_on_an_inode_the_guest_locked() {
        let fdinfo = b"\
lock:\t1: OFDLCK ADVISORY  WRITE -1 00:2a:1002 0 0\n\
lock:\t2: OFDLCK ADVISORY  READ  -1 00:2a:1003 0 EOF\n\
lock:\t3: OFDLCK ADVISORY  READ  -1 00:31:1004 0 EOF\n"
            .to_vec();
        let scans = [(
            ScanTarget::Process(task(3)),
            TableScan::Read(vec![(4, Ok(fdinfo))]),
        )];
        assert!(
            candidates(&scans, &InodeSet::default(), &InodeSet::default())
                .by_lock
                .is_empty()
        );
        // The inode alone decides, on any device: 1004 is on 00:31, as a file
        // in another btrfs subvolume may be.
        let mut locked = gate(&[1002, 1004]);
        let objects = |locked: &InodeSet| {
            let mut objects = candidates(&scans, locked, &InodeSet::default())
                .by_lock
                .into_keys()
                .map(|lock| lock.object)
                .collect::<Vec<_>>();
            objects.sort();
            objects
        };
        assert_eq!(objects(&locked), ["00:2a:1002", "00:31:1004"]);
        // The set only grows; noting an inode again changes nothing.
        locked.note(1002);
        assert_eq!(objects(&locked), ["00:2a:1002", "00:31:1004"]);
        locked.note(1003);
        assert_eq!(objects(&locked), ["00:2a:1002", "00:2a:1003", "00:31:1004"]);
        assert!(!locked.admits(&lock("00:2a:not-a-number", "0", "0")));
        // The scan filters descriptors by inode before reading their fdinfo.
        assert!(locked.has(1003) && !locked.has(1005));
    }

    /// A comparison asked, as ((task, fd), (task, fd)).
    type Asked = ((i32, i32), (i32, i32));

    /// Runs `multiplicity` to the end with `answers`, returning m(L) and the
    /// questions asked, as (task, fd) pairs.
    fn settle(
        mut multiplicity: Multiplicity,
        answers: &[Result<i64, Errno>],
    ) -> (usize, Vec<Asked>) {
        let mut asked = Vec::new();
        let mut answers = answers.iter();
        while let Some((left, right)) = multiplicity.question() {
            asked.push(((left.task.guest, left.fd), (right.task.guest, right.fd)));
            multiplicity.answer(*answers.next().expect("an answer for every question"));
        }
        assert!(answers.next().is_none(), "every answer is used");
        (multiplicity.count(usize::MAX).expect("settled"), asked)
    }

    #[test]
    fn multiplicity_needs_no_kcmp_with_one_candidate() {
        let three = vec![candidate(3, 4), candidate(3, 5), candidate(8, 4)];
        assert_eq!(settle(Multiplicity::new(vec![], true), &[]).0, 0);
        assert_eq!(
            settle(Multiplicity::new(vec![candidate(3, 4)], true), &[]).0,
            1
        );
        // A reader outside the container's PID namespace: kcmp would name
        // other tasks, so one class is all that is certain, without asking.
        assert_eq!(settle(Multiplicity::new(three, false), &[]).0, 1);
    }

    #[test]
    fn multiplicity_counts_classes_by_equal_answers_only() {
        let three = vec![candidate(3, 4), candidate(3, 5), candidate(8, 4)];
        // fd 5 is a dup of fd 4 (0); process 8's fd 4 is another description
        // (2: only "not equal" is read from the ordering).
        assert_eq!(
            settle(Multiplicity::new(three.clone(), true), &[Ok(0), Ok(2)]),
            (2, vec![((3, 5), (3, 4)), ((8, 4), (3, 4))])
        );
        // Three distinct descriptions: the third is compared with both
        // representatives.
        assert_eq!(
            settle(
                Multiplicity::new(three.clone(), true),
                &[Ok(1), Ok(2), Ok(1)]
            ),
            (
                3,
                vec![((3, 5), (3, 4)), ((8, 4), (3, 4)), ((8, 4), (3, 5))]
            )
        );
        // The third equals the second representative.
        assert_eq!(
            settle(
                Multiplicity::new(three.clone(), true),
                &[Ok(1), Ok(1), Ok(0)]
            )
            .0,
            2
        );
        // Every candidate one description (a description shared by two
        // processes' tables, CLONE_FILES or fork).
        assert_eq!(settle(Multiplicity::new(three, true), &[Ok(0), Ok(0)]).0, 1);
    }

    /// The questions do not depend on the snapshot's row count, which counts
    /// host processes' rows too: every candidate is classified, and m(L) is
    /// the class count capped at r(L) afterwards.
    #[test]
    fn multiplicity_asks_the_same_questions_whatever_the_row_count() {
        let four = vec![
            candidate(3, 4),
            candidate(3, 5),
            candidate(8, 4),
            candidate(9, 4),
        ];
        // fd 5 differs from fd 4; process 8's fd 4 equals fd 4; process 9's
        // fd 4 differs from both representatives.
        let (classes, asked) = settle(
            Multiplicity::new(four.clone(), true),
            &[Ok(1), Ok(0), Ok(1), Ok(1)],
        );
        assert_eq!(classes, 3);
        assert_eq!(asked.len(), 4);
        let mut count = Multiplicity::new(four, true);
        for answer in [Ok(1), Ok(0), Ok(1), Ok(1)] {
            count.answer(answer);
        }
        assert!(count.question().is_none());
        assert_eq!(count.count(1), Some(1));
        assert_eq!(count.count(2), Some(2));
        assert_eq!(count.count(9), Some(3));
    }

    #[test]
    fn a_failed_kcmp_caps_multiplicity_at_one() {
        let three = vec![candidate(3, 4), candidate(3, 5), candidate(8, 4)];
        assert_eq!(
            settle(
                Multiplicity::new(three.clone(), true),
                &[Err(Errno::ENOSYS)]
            )
            .0,
            1
        );
        // A failure after a class was found still caps at 1: the answers so
        // far cannot be told from the ones not asked.
        assert_eq!(
            settle(Multiplicity::new(three, true), &[Ok(1), Err(Errno::ESRCH)]).0,
            1
        );
    }

    // A snapshot, as Linux prints it to a reader in the guest's PID
    // namespace, where the guest holds a POSIX lock (pid 3) and OFD locks, a
    // host process holds others, and some locks are identical.
    const SNAPSHOT: &[u8] = b"\
1: POSIX  ADVISORY  WRITE 3 00:2a:1001 0 EOF
2: OFDLCK ADVISORY  WRITE -1 00:2a:1002 0 0
3: OFDLCK ADVISORY  READ  -1 00:2a:1005 0 EOF
3: -> POSIX  ADVISORY  WRITE 3 00:2a:1005 0 EOF
4: OFDLCK ADVISORY  READ  -1 00:2a:1005 0 EOF
4: -> POSIX  ADVISORY  WRITE 3 00:2a:1005 0 EOF
5: FLOCK  ADVISORY  WRITE -1 00:2a:1006 0 EOF
6: FLOCK  ADVISORY  WRITE 4242 00:2a:1007 0 EOF
7: FLOCK  ADVISORY  WRITE 3 00:2a:1008 0 EOF
8: OFDLCK ADVISORY  WRITE -1 00:2a:1009 0 EOF
8: -> POSIX  ADVISORY  WRITE 3 00:2a:1009 0 EOF
8: -> POSIX  ADVISORY  WRITE 4243 00:2a:1009 0 EOF
8: -> OFDLCK ADVISORY  WRITE -1 00:2a:1009 0 EOF
9: OFDLCK ADVISORY  WRITE -1 00:00:12 0 EOF
9: -> OFDLCK ADVISORY  WRITE -1 00:00:12 0 EOF
10: OFDLCK ADVISORY  WRITE -1 00:2a:1010 0 EOF
";

    fn guest(owner: i64) -> bool {
        owner == 3
    }

    fn read_lock(object: &str) -> HeldLock {
        HeldLock {
            access: "READ".to_owned(),
            ..lock(object, "0", "EOF")
        }
    }

    #[test]
    fn raw_counts_and_the_scan_gate_ignore_waiters_flocks_and_the_null_device() {
        let rows = lock_rows(SNAPSHOT).unwrap();
        assert_eq!(
            raw_ofd_counts(&rows).into_iter().collect::<Vec<_>>(),
            [
                (read_lock("00:2a:1005"), 2),
                (lock("00:2a:1002", "0", "0"), 1),
                (lock("00:2a:1009", "0", "EOF"), 1),
                (lock("00:2a:1010", "0", "EOF"), 1),
            ]
        );
        // Whether to scan depends on the guest's own lock calls only, not on
        // the rows, which host processes' locks fill too.
        assert!(!scan_needed(&InodeSet::default()));
        assert!(scan_needed(&gate(&[1009])));
        let no_ofd = lock_rows(
            b"1: POSIX  ADVISORY  WRITE 3 00:2a:1001 0 EOF\n\
2: FLOCK  ADVISORY  WRITE -1 00:2a:1006 0 EOF\n\
3: OFDLCK ADVISORY  WRITE -1 00:00:12 0 EOF\n",
        )
        .unwrap();
        assert!(raw_ofd_counts(&no_ofd).is_empty());
    }

    #[test]
    fn rows_follow_owner_multiplicity_and_the_waiter_rule() {
        let rows = lock_rows(SNAPSHOT).unwrap();
        // The guest holds 1002, one of the two identical 1005 read locks, and
        // 1009; 1010 is a host lock.
        let multiplicity = BTreeMap::from([
            (lock("00:2a:1002", "0", "0"), 1),
            (read_lock("00:2a:1005"), 1),
            (lock("00:2a:1009", "0", "EOF"), 1),
        ]);
        assert_eq!(
            keep_rows(&rows, guest, &multiplicity),
            [
                true,  // the guest's POSIX lock, by its pid
                true,  // the guest's OFD lock on 1002
                true,  // the first of two identical 1005 groups: m = 1
                false, // a guest waiter, but r(1005) = 2 > m: its group is ambiguous
                false, // the second identical 1005 group
                false, // its guest waiter
                false, // a flock with no visible owner: flock rows are owner-only
                false, // a host process's flock
                true,  // the guest's flock, by its pid
                true,  // the guest's OFD lock on 1009, r = m = 1
                true,  // the guest's POSIX waiter behind it
                false, // a host waiter
                false, // an OFD waiter: never shown, fdinfo lists none
                true,  // the KVM exemption keeps a 00:00 group
                true,  // and its waiter
                false, // a host OFD lock no guest description holds
            ]
        );
        // Both 1005 descriptions are the guest's: both groups and their guest
        // waiters stay.
        let both = BTreeMap::from([(read_lock("00:2a:1005"), 2)]);
        assert_eq!(
            keep_rows(&rows, guest, &both)[2..6],
            [true, true, true, true]
        );
        // m(L) above r(L) admits no more rows than there are.
        let more = BTreeMap::from([(lock("00:2a:1002", "0", "0"), 5)]);
        assert!(keep_rows(&rows, guest, &more)[1]);
        // No multiplicity (no scan, or one that must not be trusted) hides
        // every OFD row and its waiters; owner-decided and 00:00 rows stay.
        assert_eq!(
            keep_rows(&rows, guest, &BTreeMap::new()),
            [
                true, false, false, false, false, false, false, false, true, false, false, false,
                false, true, true, false,
            ]
        );
    }

    #[test]
    fn the_scan_decides_a_snapshot_end_to_end() {
        // Process 3 holds 1005 through fd 4 and a dup at fd 5; its thread 5
        // has a private table holding another 1005 description at fd 4.
        // Process 8 cannot be read. The guest locked 1002 and 1005 only.
        let rows = lock_rows(SNAPSHOT).unwrap();
        let (targets, mut hidden, _) = select(
            vec![live(3, true, &[5]), live(8, true, &[])],
            true,
            |task| {
                Some(match task {
                    5 => 'B',
                    _ => 'A',
                })
            },
        );
        let read = b"lock:\t1: OFDLCK ADVISORY  READ  -1 00:2a:1005 0 EOF\n\
lock:\t2: OFDLCK ADVISORY  WRITE -1 00:2a:1010 0 EOF\n"
            .to_vec();
        let scans = targets
            .iter()
            .map(|target| {
                let scan = match target {
                    ScanTarget::Process(process) if process.guest == 3 => {
                        TableScan::Read(vec![(4, Ok(read.clone())), (5, Ok(read.clone()))])
                    }
                    ScanTarget::Thread { .. } => TableScan::Read(vec![(4, Ok(read.clone()))]),
                    ScanTarget::Process(_) => TableScan::Unreadable(Unreadable::Failed {
                        call: "openat",
                        errno: Errno::EACCES,
                    }),
                };
                (*target, scan)
            })
            .collect::<Vec<_>>();
        let found = candidates(&scans, &gate(&[1002, 1005]), &InodeSet::default());
        hidden.extend(found.hidden);
        assert_eq!(hidden.len(), 1);
        let raw = raw_ofd_counts(&rows);
        let mut multiplicity = BTreeMap::new();
        for (held, candidates) in found.by_lock {
            let mut count = Multiplicity::new(candidates, true);
            while let Some((left, right)) = count.question() {
                // fd 5 of process 3 is a dup of its fd 4; the thread's fd 4
                // is another description.
                let same = left.task == right.task;
                count.answer(Ok(if same { 0 } else { 1 }));
            }
            let raw = raw.get(&held).copied().unwrap_or(0);
            multiplicity.insert(held, count.count(raw).unwrap());
        }
        assert_eq!(
            multiplicity.clone().into_iter().collect::<Vec<_>>(),
            [(read_lock("00:2a:1005"), 2)]
        );
        let keep = keep_rows(&rows, guest, &multiplicity);
        // Both 1005 groups and their guest waiters stay; 1002 was never seen
        // in a readable table, and 1010 is on an inode the guest never locked.
        assert_eq!(keep[1..6], [false, true, true, true, true]);
        assert!(!keep[15]);
    }
}
