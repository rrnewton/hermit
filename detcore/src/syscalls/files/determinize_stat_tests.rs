/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! What `Detcore::determinize_stat` reads of the configuration: whether the
//! run records or replays (`recordreplay_modes`) and its epoch. It reads them
//! without copying the configuration, whose container mount lists made every
//! stat-family call allocate once per mount.

use std::path::Path;

use chrono::TimeZone;
use chrono::Utc;
use reverie::Errno;
use reverie::Error;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::LocalMemory;
use reverie::syscalls::StatxMask;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Timespec;

use super::ProcfsEntry;
use super::rendered_entry;
use crate::Detcore;
use crate::config::Config;
use crate::procfs::RenderedFiles;
use crate::procfs_inode;
use crate::stat::DetStat;
use crate::tool_global::GlobalState;
use crate::types::DetInode;
use crate::types::DetPid;

/// A thread whose RPCs reach a real global tool, and which does nothing else.
struct StatGuest<'a> {
    config: &'a Config,
    global: &'a GlobalState,
    thread: crate::ThreadState<()>,
    sender: Tid,
}

struct NoStack;
struct NoStackGuard;
impl Drop for NoStackGuard {
    fn drop(&mut self) {}
}
impl reverie::Stack for NoStack {
    type StackGuard = NoStackGuard;
    fn size(&self) -> usize {
        panic!("determinizing a stat must not use the guest stack")
    }
    fn capacity(&self) -> usize {
        panic!("determinizing a stat must not use the guest stack")
    }
    fn push<'stack, T>(&mut self, _: T) -> Addr<'stack, T> {
        panic!("determinizing a stat must not use the guest stack")
    }
    fn reserve<'stack, T>(&mut self) -> AddrMut<'stack, T> {
        panic!("determinizing a stat must not use the guest stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("determinizing a stat must not use the guest stack")
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for StatGuest<'_> {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.global.receive_rpc(self.sender, message).await
    }
    fn config(&self) -> &Config {
        self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for StatGuest<'_> {
    type Memory = LocalMemory;
    type Stack = NoStack;

    fn tid(&self) -> Tid {
        self.sender
    }
    fn pid(&self) -> Tid {
        self.sender
    }
    fn ppid(&self) -> Option<Tid> {
        None
    }
    fn memory(&self) -> Self::Memory {
        LocalMemory::new()
    }
    fn thread_state(&self) -> &crate::ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("determinizing a stat must not read registers")
    }
    async fn stack(&mut self) -> Self::Stack {
        panic!("determinizing a stat must not use the guest stack")
    }
    async fn daemonize(&mut self) {
        panic!("determinizing a stat must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        panic!("determinizing a stat must not inject {:?}", call.number())
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("determinizing a stat must not retire the guest")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("determinizing a stat must not set a timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("determinizing a stat must not set a timer")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("determinizing a stat must not read the clock")
    }
}

/// The rendered procfs file the cases name, by its path within procfs.
const UUID_WITHIN: &str = "/sys/kernel/random/uuid";

/// A host stat of a rendered procfs file as the kernel reports it: a regular
/// file, mode 0444, one link, and a modification time that is not canonical
/// (`CANONICAL_FILE_MTIME_SECONDS`), so that a file Detcore numbers reports
/// the epoch.
fn host_uuid_stat() -> DetStat {
    let mut stat = DetStat {
        mode: libc::S_IFREG | 0o444,
        dev: 0x16,
        inode: 4242,
        ..DetStat::default()
    };
    let host_time = Timespec {
        tv_sec: 1_700_000_000,
        tv_nsec: 5,
    };
    stat.atime = host_time;
    stat.btime = host_time;
    stat.ctime = host_time;
    stat.mtime = host_time;
    stat
}

/// A configuration like a container's: mount-table rewrites present, which
/// `determinize_stat` must not need, and the two fields it reads.
fn config(recordreplay_modes: bool, epoch: chrono::DateTime<Utc>) -> Config {
    Config {
        recordreplay_modes,
        epoch,
        virtualize_metadata: true,
        mountinfo_device_rewrites: (0..64).map(|i| (i, i + 1000)).collect(),
        mountinfo_mount_ids: (100..164).collect(),
        mount_id_assignment_order: (100..164).collect(),
        ..Config::default()
    }
}

/// `rendered_entry` names a rendered procfs file only outside record and
/// replay, where Detcore renders procfs files at all.
#[test]
fn rendered_entry_names_rendered_files_only_outside_record_and_replay() {
    let procfs = ProcfsEntry::At(UUID_WITHIN.into());
    let stat = host_uuid_stat();
    let path = rendered_entry(&procfs, &stat, false, RenderedFiles::ALL)
        .expect("the uuid file is rendered");
    assert_eq!(path, Path::new("/proc/sys/kernel/random/uuid"));
    assert_eq!(
        rendered_entry(&procfs, &stat, true, RenderedFiles::ALL),
        None
    );
}

/// For each record/replay mode and two epochs, the determinized stat of a
/// rendered procfs file and of a container stdio stream (an inode override):
/// atime, ctime and btime are the configured epoch; the stream keeps its
/// inode and reports the epoch as its mtime; the procfs file reports its
/// rendered inode outside record and replay, and a pool inode in them.
#[tokio::test]
async fn determinize_stat_reads_the_mode_and_epoch_it_is_configured_with() {
    let epochs = [
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(1999, 12, 31, 23, 59, 58).unwrap(),
    ];
    let rendered = procfs_inode::rendered_inode(Path::new("/proc/sys/kernel/random/uuid"));
    for recordreplay_modes in [false, true] {
        for epoch in epochs {
            let config = config(recordreplay_modes, epoch);
            let epoch_tp = Timespec {
                tv_sec: epoch.timestamp(),
                tv_nsec: epoch.timestamp_subsec_nanos() as i64,
            };
            let process = DetPid::from_raw(7);
            let global = GlobalState::init_global_state(&config).await;
            let tool = Detcore::new(Tid::from_raw(process.as_raw()), &config);
            let mut thread = crate::ThreadState::new(process, &config, ());
            thread.detpid = Some(process);
            let mut guest = StatGuest {
                config: &config,
                global: &global,
                thread,
                sender: Tid::from_raw(process.as_raw()),
            };
            let case = format!("recordreplay_modes={recordreplay_modes} epoch={epoch}");

            let stream = tool
                .determinize_stat(
                    &mut guest,
                    host_uuid_stat(),
                    Some(DetInode::mint(3)),
                    ProcfsEntry::NotProcfs,
                )
                .await
                .unwrap();
            assert_eq!(stream.inode, 3, "{case}");
            for (field, time) in [
                ("atime", stream.atime),
                ("btime", stream.btime),
                ("ctime", stream.ctime),
                ("mtime", stream.mtime),
            ] {
                assert_eq!(time, epoch_tp, "stream {field}: {case}");
            }

            let procfs = tool
                .determinize_stat(
                    &mut guest,
                    host_uuid_stat(),
                    None,
                    ProcfsEntry::At(UUID_WITHIN.into()),
                )
                .await
                .unwrap();
            if recordreplay_modes {
                assert_ne!(procfs.inode, rendered, "{case}");
                assert_ne!(procfs.inode, 4242, "a host inode leaked: {case}");
            } else {
                assert_eq!(procfs.inode, rendered, "{case}");
            }
            assert!(procfs.mask.contains(StatxMask::STATX_MTIME), "{case}");
            for (field, time) in [
                ("atime", procfs.atime),
                ("btime", procfs.btime),
                ("ctime", procfs.ctime),
                ("mtime", procfs.mtime),
            ] {
                assert_eq!(time, epoch_tp, "procfs {field}: {case}");
            }
        }
    }
}

/// A Detcore placeholder (a memfd standing in for a rendered procfs file,
/// however the guest reached it) is answered with the rendered file's
/// metadata outside record and replay, and like any other file in them: the
/// third place `determinize_stat` reads the record/replay flag.
///
/// The placeholder registry is per tool process; Nextest runs each test in a
/// process of its own, and the identity below is one no kernel object of the
/// test process has.
#[tokio::test]
async fn determinize_stat_answers_a_placeholder_only_outside_record_and_replay() {
    const PLACEHOLDER_DEV: u64 = 0x0bad_f00d;
    const PLACEHOLDER_INO: u64 = 0x7d00_0001;
    let uuid = Path::new("/proc/sys/kernel/random/uuid");
    super::rendered_procfs::register_placeholder(PLACEHOLDER_DEV, PLACEHOLDER_INO, uuid);
    let rendered = procfs_inode::rendered_inode(uuid);
    let epoch = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let epoch_tp = Timespec {
        tv_sec: epoch.timestamp(),
        tv_nsec: epoch.timestamp_subsec_nanos() as i64,
    };
    for recordreplay_modes in [false, true] {
        let config = config(recordreplay_modes, epoch);
        let process = DetPid::from_raw(7);
        let global = GlobalState::init_global_state(&config).await;
        let tool = Detcore::new(Tid::from_raw(process.as_raw()), &config);
        let mut thread = crate::ThreadState::new(process, &config, ());
        thread.detpid = Some(process);
        let mut guest = StatGuest {
            config: &config,
            global: &global,
            thread,
            sender: Tid::from_raw(process.as_raw()),
        };
        let case = format!("recordreplay_modes={recordreplay_modes}");
        let memfd = DetStat {
            mode: libc::S_IFREG | 0o600,
            dev: PLACEHOLDER_DEV,
            inode: PLACEHOLDER_INO,
            ..host_uuid_stat()
        };
        let stat = tool
            .determinize_stat(&mut guest, memfd, None, ProcfsEntry::NotProcfs)
            .await
            .unwrap();
        if recordreplay_modes {
            assert_ne!(stat.inode, rendered, "{case}");
            assert_ne!(stat.inode, PLACEHOLDER_INO, "a host inode leaked: {case}");
            assert_eq!(stat.mode, libc::S_IFREG | 0o600, "{case}");
        } else {
            assert_eq!(stat.inode, rendered, "{case}");
            assert_eq!(stat.mode & libc::S_IFMT, libc::S_IFREG, "{case}");
            assert_eq!(stat.mode & 0o7777, 0o444, "{case}");
        }
        for (field, time) in [
            ("atime", stat.atime),
            ("btime", stat.btime),
            ("ctime", stat.ctime),
            ("mtime", stat.mtime),
        ] {
            assert_eq!(time, epoch_tp, "placeholder {field}: {case}");
        }
    }
}
