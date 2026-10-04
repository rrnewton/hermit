/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic views of kernel namespace metadata.

use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixDatagram;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;

use super::deterministic_stdio_inode;
use super::deterministic_stdio_inode_for_resource;
use crate::record_or_replay::RecordOrReplay;
use crate::tool_global::determinize_inode;
use crate::tool_local::Detcore;
use crate::types::DetInode;
use crate::types::RawFileId;
use crate::types::RawInode;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#877)
fn is_proc_id(component: &OsStr) -> bool {
    component == "self"
        || component == "thread-self"
        || component.to_str().is_some_and(|value| {
            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
        })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#877)
fn canonical_namespace_name(name: &OsStr) -> Option<&'static [u8]> {
    match name.to_str()? {
        "cgroup" => Some(b"cgroup:[4026531835]"),
        "ipc" => Some(b"ipc:[4026531839]"),
        "mnt" => Some(b"mnt:[4026531841]"),
        "net" => Some(b"net:[4026531840]"),
        "pid" | "pid_for_children" => Some(b"pid:[4026531836]"),
        "time" | "time_for_children" => Some(b"time:[4026531834]"),
        "user" => Some(b"user:[4026531837]"),
        "uts" => Some(b"uts:[4026531838]"),
        _ => None,
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#877)
fn canonical_namespace_target(path: &Path) -> Option<&'static [u8]> {
    if !path.is_absolute() {
        return None;
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => parts.push(part),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => return None,
        }
    }

    let namespace = match parts.as_slice() {
        [proc, subject, ns, namespace] if *proc == "proc" && is_proc_id(subject) && *ns == "ns" => {
            namespace
        }
        [proc, subject, task, tid, ns, namespace]
            if *proc == "proc"
                && is_proc_id(subject)
                && *task == "task"
                && is_proc_id(tid)
                && *ns == "ns" =>
        {
            namespace
        }
        _ => return None,
    };
    canonical_namespace_name(namespace)
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-972): Review proc-fd path alias coverage.
fn normalized_absolute_parts(path: &Path) -> Option<Vec<&OsStr>> {
    if !path.is_absolute() {
        return None;
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Prefix(_) => return None,
        }
    }
    Some(parts)
}

fn decimal_u32(component: &OsStr) -> Option<u32> {
    let value = component.to_str()?;
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())?
}

fn decimal_fd(component: &OsStr) -> Option<i32> {
    let value = component.to_str()?;
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())?
}

/// Returns the optional numeric proc subject and the descriptor number.
fn proc_fd_target(path: &Path) -> Option<(Option<u32>, i32)> {
    let parts = normalized_absolute_parts(path)?;
    match parts.as_slice() {
        [dev, fd_dir, fd] if *dev == "dev" && *fd_dir == "fd" => Some((None, decimal_fd(fd)?)),
        [proc, subject, fd_dir, fd] if *proc == "proc" && *fd_dir == "fd" => {
            let subject = match subject.to_str()? {
                "self" | "thread-self" => None,
                _ => Some(decimal_u32(subject)?),
            };
            Some((subject, decimal_fd(fd)?))
        }
        _ => None,
    }
}

// TODO-HUMAN-REVIEW(PR-1079): Review numeric virtual-self proc-fd path rewriting.
fn host_self_proc_fd_alias(path: &Path, current_pid: i64) -> Option<PathBuf> {
    let (Some(subject), fd) = proc_fd_target(path)? else {
        return None;
    };
    (i64::from(subject) == current_pid).then(|| PathBuf::from(format!("/proc/self/fd/{fd}")))
}

#[derive(Debug, Eq, PartialEq)]
struct AnonymousProcFdIdentity {
    kind: &'static str,
    raw_inode: RawInode,
}

// Long enough for `socket:[` + every decimal u64 inode + `]`.
const ANONYMOUS_PROC_FD_TARGET_CAPACITY: usize = 32;

fn needs_anonymous_proc_fd_scratch(buffer_present: bool, buffer_len: usize) -> bool {
    buffer_present && buffer_len != 0 && buffer_len < ANONYMOUS_PROC_FD_TARGET_CAPACITY
}

/// Recognize only a complete kernel pipe/socket symlink target.
fn anonymous_proc_fd_identity(target: &[u8]) -> Option<AnonymousProcFdIdentity> {
    for (kind, prefix) in [
        ("pipe", b"pipe:[".as_slice()),
        ("socket", b"socket:[".as_slice()),
    ] {
        let Some(digits) = target
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(b"]"))
        else {
            continue;
        };
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            continue;
        }
        let raw_inode = std::str::from_utf8(digits).ok()?.parse().ok()?;
        return Some(AnonymousProcFdIdentity { kind, raw_inode });
    }
    None
}

/// Raw devices of the kernel-internal filesystems that back every anonymous
/// pipe (pipefs) and every socket (sockfs).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AnonymousObjectDevices {
    pipe: u64,
    socket: u64,
}

/// The outcome of the one probe `init_anonymous_object_devices` runs in this
/// address space: the devices, or the reason the probe failed.
static ANONYMOUS_OBJECT_DEVICES: OnceLock<Result<AnonymousObjectDevices, String>> = OnceLock::new();

/// Create, `fstat` and close one pipe and one socket, and return the devices
/// they report.
///
/// A `pipe:[N]` or `socket:[N]` link names only an inode, and the determinized
/// inode pool is keyed on device and inode together
/// (<https://github.com/rrnewton/hermit/issues/3307>). The kernel has exactly
/// one pipefs superblock and one sockfs superblock: each is a single internal
/// mount made at boot (`kern_mount` in `init_pipe_fs` in fs/pipe.c and in
/// `sock_init` in net/socket.c), every pipe and every socket inode is
/// allocated on it, and `stat` reports the superblock's `s_dev` untranslated
/// in every mount, network and user namespace. So a pipe and a socket created
/// here report the same devices as every guest pipe and socket, and as an
/// `fstat` of one of the guest's own descriptors.
fn probe_anonymous_object_devices() -> Result<AnonymousObjectDevices, String> {
    let probe_error =
        |what: &str, error: std::io::Error| format!("failed to probe the {what} device: {error}");
    let (reader, writer) = std::io::pipe().map_err(|error| probe_error("pipefs", error))?;
    drop(writer);
    let pipe = File::from(OwnedFd::from(reader))
        .metadata()
        .map_err(|error| probe_error("pipefs", error))?
        .dev();
    let socket = UnixDatagram::unbound().map_err(|error| probe_error("sockfs", error))?;
    let socket = File::from(OwnedFd::from(socket))
        .metadata()
        .map_err(|error| probe_error("sockfs", error))?
        .dev();
    Ok(AnonymousObjectDevices { pipe, socket })
}

/// Probe the pipefs and sockfs devices once for this address space, and cache
/// the outcome, a failure included. Called by `Detcore`'s `Tool::new`, and
/// never while a guest system call is being handled.
///
/// ⚠️ THE PROBE MUST RUN BEFORE THE GUEST CAN RUN CONCURRENTLY WITH IT.
/// detcore-dbt, detcore-sabre, in-guest LiteInst (`HERMIT_LITEINST_IN_GUEST`)
/// and e9patch's in-guest tool host embed Detcore in the guest process, where
/// the probe's descriptors come from the GUEST's descriptor table. Probing
/// while another guest thread runs -- one blocked in `recvmsg` under
/// `BlockingExternalIO`, say -- would let that thread's next descriptor
/// (an `SCM_RIGHTS` delivery, an `accept`) land on a number that depends on
/// how the two interleaved. Each of those hosts constructs the tool before
/// the process can have a second thread: detcore-dbt at the process's first
/// intercepted system call, before a clone-family call proceeds
/// (detcore-dbt/src/lib.rs, the `runtime.tool.get_or_init` in its system-call
/// handler); detcore-sabre when the plugin initializes (reverie-sabre
/// `reverie_adapter.rs`, `connect_with_root_initializer`); in-guest LiteInst
/// and e9patch at tool installation (`tool_host.rs` in reverie-liteinst and
/// reverie-e9patch), which runs "before application-created threads". A
/// forked child copies this static already initialized, and `execve` both
/// resets it and unshares the descriptor table, so every probe sees a
/// single-threaded process whose descriptors are its own, and closes what it
/// opened before returning. The tracer-side backends (ptrace, e9patch
/// preprocessing with ptrace, tracer-side LiteInst) and KVM probe in the
/// Hermit process, never in a guest descriptor table.
///
/// A failure (`EMFILE`, `ENFILE`, `ENOMEM`) is cached rather than retried:
/// whether a later probe would succeed depends on host state, and a guest
/// that reads an unconfirmed link then gets an error from
/// `anonymous_object_devices` rather than an identity on a made-up device.
pub(crate) fn init_anonymous_object_devices() {
    ANONYMOUS_OBJECT_DEVICES.get_or_init(probe_anonymous_object_devices);
}

/// The devices from a cached probe outcome: an error when the probe failed
/// or never ran, never a substitute device.
fn cached_anonymous_object_devices(
    cached: Option<&Result<AnonymousObjectDevices, String>>,
) -> Result<AnonymousObjectDevices, Error> {
    match cached {
        Some(Ok(devices)) => Ok(*devices),
        Some(Err(error)) => Err(Error::Tool(anyhow::anyhow!(
            "cannot key an unconfirmed /proc/<pid>/fd link: {error}"
        ))),
        None => Err(Error::Tool(anyhow::anyhow!(
            "cannot key an unconfirmed /proc/<pid>/fd link: the pipefs and sockfs devices were \
             not probed when the Detcore tool was constructed"
        ))),
    }
}

/// The pipefs and sockfs devices `init_anonymous_object_devices` probed.
/// Never probes, so never touches a descriptor table.
fn anonymous_object_devices() -> Result<AnonymousObjectDevices, Error> {
    cached_anonymous_object_devices(ANONYMOUS_OBJECT_DEVICES.get())
}

impl AnonymousProcFdIdentity {
    /// Whether a `stat` of the link describes the object this link target
    /// names: the same kind of object and the same inode.
    fn matches_stat(&self, mode: libc::mode_t, inode: RawInode) -> bool {
        let file_type = match self.kind {
            "pipe" => libc::S_IFIFO,
            "socket" => libc::S_IFSOCK,
            kind => unreachable!("anonymous_proc_fd_identity never yields kind {kind:?}"),
        };
        mode & libc::S_IFMT == file_type && inode == self.raw_inode
    }

    /// The raw identity of the object this link names.
    fn raw_file_id(&self, devices: AnonymousObjectDevices) -> RawFileId {
        let device = match self.kind {
            "pipe" => devices.pipe,
            "socket" => devices.socket,
            kind => unreachable!("anonymous_proc_fd_identity never yields kind {kind:?}"),
        };
        RawFileId::new(device, self.raw_inode)
    }
}

/// Match a raw identity against cached current-process inherited-stdio identities.
/// Callers exclude ordinary replacement objects before filling this array.
///
/// When stdio descriptors alias, the LOWEST matching descriptor wins, as in
/// the maps sanitizer's `stdio_by_raw_file` table. The tracer-side backends
/// cache the tracer's `fstat(0)` for all three descriptors, so the stdin object
/// matches every slot, and only the lowest agrees with the guest's `fstat(0)`
/// and with its own `/proc/self/fd/0` link.
fn deterministic_stdio_inode_for_raw(
    raw_inode: RawFileId,
    stdio_raw_inodes: &[Option<RawFileId>; 3],
) -> Option<DetInode> {
    stdio_raw_inodes
        .iter()
        .position(|cached| *cached == Some(raw_inode))
        .and_then(|fd| deterministic_stdio_inode(fd as i32))
}

fn canonical_anonymous_proc_fd_target(
    identity: &AnonymousProcFdIdentity,
    inode: DetInode,
    buffer_len: usize,
) -> Vec<u8> {
    let mut target = format!("{}:[{}]", identity.kind, inode.as_raw()).into_bytes();
    target.truncate(buffer_len);
    target
}

/// Another process's `/proc/<subject>/fd/<fd>` link, as the guest named it.
#[derive(Clone, Copy, Debug)]
struct OtherProcFdLink {
    subject: u32,
    fd: i32,
}

impl<T: RecordOrReplay> Detcore<T> {
    /// The raw identity of the object behind another process's pipe or socket
    /// link, from a `stat` of the link performed by the guest.
    ///
    /// `stat` follows the link to the object itself, so it reports the pipefs
    /// or sockfs device along with the inode, without creating a descriptor
    /// anywhere. Linux applies the same ptrace-read access check to following
    /// the link as to reading it, so it succeeds wherever the readlink did.
    /// `None` when the stat fails or describes a different object -- the
    /// descriptor was closed or replaced after the readlink, or the guest has
    /// no `/proc` of its own, as under the replayer's chroot.
    async fn other_proc_fd_link_identity<G>(
        &self,
        guest: &mut G,
        link: OtherProcFdLink,
        identity: &AnonymousProcFdIdentity,
    ) -> Result<Option<RawFileId>, Error>
    where
        G: Guest<Self>,
    {
        let path = format!("/proc/{}/fd/{}", link.subject, link.fd);
        let Some(stat) = self.stat_guest_path(guest, path.as_bytes()).await? else {
            return Ok(None);
        };
        Ok(identity
            .matches_stat(stat.st_mode, stat.st_ino)
            .then(|| RawFileId::new(stat.st_dev, stat.st_ino)))
    }

    /// Rewrite another process's `pipe:[N]` or `socket:[N]` link target to
    /// name the object's deterministic inode; `None` for any other target.
    ///
    /// The object is keyed on its device and inode, as an `fstat` of it is:
    /// the device comes from the guest's `stat` of the link
    /// (`other_proc_fd_link_identity`), or, when that cannot confirm the
    /// object, from the devices probed when the tool was constructed
    /// (`init_anonymous_object_devices`). If that probe failed, the readlink
    /// fails with an error rather than keying the link on a device no `fstat`
    /// reports.
    ///
    /// Without `virtualize_metadata` -- `hermit record` and `hermit replay`
    /// -- neither is consulted, and the object is keyed on its inode alone,
    /// on device 0, with a stdio descriptor matched by inode alone. There the
    /// replayer returns the RECORDED target, so the inode is the recording
    /// host's, while a `stat` of the link at replay time follows whatever the
    /// replay host has at that descriptor (nothing, under the replayer's
    /// chroot without `/proc`) and the probe reports the replay host's
    /// devices. Keying on either would make the replayed name depend on the
    /// replay environment rather than on the recording. No filesystem has
    /// device 0 (Linux numbers anonymous devices from minor 1), so the key
    /// cannot alias a file, and keying on the inode alone is what this
    /// rewrite did before the pool was keyed on devices
    /// (<https://github.com/rrnewton/hermit/issues/3307>). The guest's own
    /// links use the same device-0 key in this mode
    /// (`own_proc_fd_link_identity`).
    ///
    /// ⚠️ THE STDIO MATCH IS STILL AGAINST THE RUNNING TRACER'S STDIN. The
    /// cached stdio stat is the tracer's `fstat(0)`, taken in each run, so a
    /// recorded link to the recording's stdin object names the stdio inode
    /// only when the replayer's stdin has the same inode number. This is
    /// unchanged from before the pool was keyed on devices.
    async fn canonicalize_other_proc_fd_target<G>(
        &self,
        guest: &mut G,
        link: OtherProcFdLink,
        raw_target: &[u8],
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
    ) -> Result<Option<i64>, Error>
    where
        G: Guest<Self>,
    {
        let Some(identity) = anonymous_proc_fd_identity(raw_target) else {
            return Ok(None);
        };
        let virtualize_metadata = guest.config().virtualize_metadata;
        let mut stdio_raw_inodes = [None; 3];
        for fd in libc::STDIN_FILENO..=libc::STDERR_FILENO {
            stdio_raw_inodes[fd as usize] = guest
                .thread_state()
                .with_detfd(fd, |detfd| {
                    deterministic_stdio_inode_for_resource(fd, detfd.resource())?;
                    detfd.stat().map(|stat| {
                        if virtualize_metadata {
                            stat.raw_file_id()
                        } else {
                            RawFileId::new(0, stat.inode)
                        }
                    })
                })
                .ok()
                .flatten();
        }
        let raw_file = if virtualize_metadata {
            match self
                .other_proc_fd_link_identity(guest, link, &identity)
                .await?
            {
                Some(raw_file) => raw_file,
                None => identity.raw_file_id(anonymous_object_devices()?),
            }
        } else {
            RawFileId::new(0, identity.raw_inode)
        };
        let inode = match deterministic_stdio_inode_for_raw(raw_file, &stdio_raw_inodes) {
            Some(inode) => inode,
            None => determinize_inode(guest, raw_file).await.0,
        };
        let target = canonical_anonymous_proc_fd_target(&identity, inode, buffer_len);
        let buffer = buffer.ok_or(Errno::EFAULT)?;
        guest.memory().write_exact(buffer.cast(), &target)?;
        Ok(Some(target.len() as i64))
    }

    /// The kind and raw identity of the object behind one of the guest's OWN
    /// descriptors, for its `/proc/self/fd/<fd>` link or its own numeric
    /// `/proc/<pid>/fd/<fd>`, given the target bytes the guest's readlink
    /// returned; `None` when the object is neither a pipe nor a socket.
    ///
    /// The identity is in the same namespace as the one
    /// `canonicalize_other_proc_fd_target` uses for another process's link to
    /// the same object, so the two aliases of one pipe or socket name one
    /// deterministic inode. With `virtualize_metadata` both are the object's
    /// device and inode, here from an `fstat` of the descriptor. Without it
    /// -- `hermit record` and `hermit replay` -- both are the inode the link
    /// target names, on device 0. At replay the target is the RECORDED one,
    /// while an `fstat` describes whatever the replayer holds at that
    /// descriptor (a placeholder where it does not recreate the object), so
    /// keying on the target is what keeps the replayed name equal to the
    /// recorded one and to the other alias.
    ///
    /// ⚠️ A TARGET TRUNCATED BY A SHORT BUFFER DOES NOT PARSE, and then the
    /// `fstat` decides without `virtualize_metadata` too, keyed on its inode
    /// on device 0. Under `hermit record` that is the inode the target names.
    /// At replay it is the replayer's descriptor, as it was before the pool
    /// was keyed on devices (<https://github.com/rrnewton/hermit/issues/3307>),
    /// so a truncated read of the guest's own link can still name a different
    /// inode at replay than in the recording.
    async fn own_proc_fd_link_identity<G>(
        &self,
        guest: &mut G,
        fd: i32,
        observed_target: &[u8],
    ) -> Result<Option<(&'static str, RawFileId)>, Error>
    where
        G: Guest<Self>,
    {
        let virtualize_metadata = guest.config().virtualize_metadata;
        if !virtualize_metadata && let Some(identity) = anonymous_proc_fd_identity(observed_target)
        {
            return Ok(Some((identity.kind, RawFileId::new(0, identity.raw_inode))));
        }
        let stat = self.inject_fstat(guest, fd).await?;
        let kind = match stat.st_mode & libc::S_IFMT {
            libc::S_IFIFO => "pipe",
            libc::S_IFSOCK => "socket",
            _ => return Ok(None),
        };
        let device = if virtualize_metadata { stat.st_dev } else { 0 };
        Ok(Some((kind, RawFileId::new(device, stat.st_ino))))
    }

    /// Canonicalize a pipe/socket link belonging to another virtual process.
    /// The target descriptor is not in the caller's table, so its raw readlink
    /// bytes are the only safe identity evidence available here.
    async fn canonicalize_other_proc_fd_readlink<G>(
        &self,
        guest: &mut G,
        link: OtherProcFdLink,
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
        result: i64,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
    {
        let buffer = buffer.expect("a successful readlink requires a non-null buffer");
        let observed_len = usize::try_from(result)
            .expect("a positive readlink result must fit usize")
            .min(buffer_len);
        let mut observed = vec![0; observed_len];
        guest.memory().read_exact(buffer.cast(), &mut observed)?;
        Ok(self
            .canonicalize_other_proc_fd_target(guest, link, &observed, Some(buffer), buffer_len)
            .await?
            .unwrap_or(result))
    }

    async fn canonicalize_namespace_readlink_result<G>(
        &self,
        guest: &mut G,
        path: PathBuf,
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
        result: i64,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
    {
        if result <= 0 {
            return Ok(result);
        }

        let target = if let Some(target) = canonical_namespace_target(&path) {
            target.to_vec()
        } else if let Some((subject, fd)) = proc_fd_target(&path) {
            if let Some(subject) = subject {
                let current_pid = guest.inject(syscalls::Getpid::new()).await?;
                if current_pid != i64::from(subject) {
                    return self
                        .canonicalize_other_proc_fd_readlink(
                            guest,
                            OtherProcFdLink { subject, fd },
                            buffer,
                            buffer_len,
                            result,
                        )
                        .await;
                }
            }

            let buffer = buffer.expect("a successful readlink requires a non-null buffer");
            let observed_len = usize::try_from(result)
                .expect("a positive readlink result must fit usize")
                .min(buffer_len);
            let mut observed = vec![0; observed_len];
            guest.memory().read_exact(buffer.cast(), &mut observed)?;
            let Some((kind, raw_file)) =
                self.own_proc_fd_link_identity(guest, fd, &observed).await?
            else {
                return Ok(result);
            };
            let inode_override = guest
                .thread_state()
                .with_detfd(fd, |detfd| {
                    deterministic_stdio_inode_for_resource(fd, detfd.resource())
                })
                .ok()
                .flatten();
            let inode = match inode_override {
                Some(inode) => inode,
                None => determinize_inode(guest, raw_file).await.0,
            };
            format!("{kind}:[{inode}]").into_bytes()
        } else {
            return Ok(result);
        };

        let written = target.len().min(buffer_len);
        let buffer = buffer.expect("a successful readlink requires a non-null buffer");
        guest
            .memory()
            .write_exact(buffer.cast(), &target[..written])?;
        Ok(written as i64)
    }
    async fn write_other_proc_fd_target<G>(
        &self,
        guest: &mut G,
        link: OtherProcFdLink,
        raw_target: &[u8],
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
    {
        if let Some(result) = self
            .canonicalize_other_proc_fd_target(guest, link, raw_target, buffer, buffer_len)
            .await?
        {
            return Ok(result);
        }
        let written = raw_target.len().min(buffer_len);
        let buffer = buffer.ok_or(Errno::EFAULT)?;
        guest
            .memory()
            .write_exact(buffer.cast(), &raw_target[..written])?;
        Ok(written as i64)
    }

    async fn finish_other_proc_fd_readlink<G>(
        &self,
        guest: &mut G,
        link: OtherProcFdLink,
        call: syscalls::Readlink,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
    {
        let buffer = call.buf();
        let buffer_len = call.bufsize();
        if !needs_anonymous_proc_fd_scratch(buffer.is_some(), buffer_len) {
            let result = self.record_or_replay(guest, call).await?;
            if result <= 0 {
                return Ok(result);
            }
            return self
                .canonicalize_other_proc_fd_readlink(guest, link, buffer, buffer_len, result)
                .await;
        }

        let mut stack = guest.stack().await;
        let scratch = stack
            .reserve::<[u8; ANONYMOUS_PROC_FD_TARGET_CAPACITY]>()
            .cast::<libc::c_char>();
        let guard = stack.commit()?;
        let physical_call = call
            .with_buf(Some(scratch))
            .with_bufsize(ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        let result = self.record_or_replay(guest, physical_call).await?;
        let length = usize::try_from(result)
            .expect("a successful readlink result must fit usize")
            .min(ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        let mut raw_target = vec![0; length];
        guest.memory().read_exact(scratch.cast(), &mut raw_target)?;
        drop(guard);
        self.write_other_proc_fd_target(guest, link, &raw_target, buffer, buffer_len)
            .await
    }

    async fn finish_other_proc_fd_readlinkat<G>(
        &self,
        guest: &mut G,
        link: OtherProcFdLink,
        call: syscalls::Readlinkat,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
    {
        let buffer = call.buf();
        let buffer_len = call.buf_len();
        if !needs_anonymous_proc_fd_scratch(buffer.is_some(), buffer_len) {
            let result = self.record_or_replay(guest, call).await?;
            if result <= 0 {
                return Ok(result);
            }
            return self
                .canonicalize_other_proc_fd_readlink(guest, link, buffer, buffer_len, result)
                .await;
        }

        let mut stack = guest.stack().await;
        let scratch = stack
            .reserve::<[u8; ANONYMOUS_PROC_FD_TARGET_CAPACITY]>()
            .cast::<libc::c_char>();
        let guard = stack.commit()?;
        let physical_call = call
            .with_buf(Some(scratch))
            .with_buf_len(ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        let result = self.record_or_replay(guest, physical_call).await?;
        let length = usize::try_from(result)
            .expect("a successful readlinkat result must fit usize")
            .min(ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        let mut raw_target = vec![0; length];
        guest.memory().read_exact(scratch.cast(), &mut raw_target)?;
        drop(guard);
        self.write_other_proc_fd_target(guest, link, &raw_target, buffer, buffer_len)
            .await
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#877)
    async fn finish_namespace_readlink<G, S>(
        &self,
        guest: &mut G,
        path: PathBuf,
        buffer: Option<AddrMut<'_, libc::c_char>>,
        buffer_len: usize,
        syscall: S,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
        S: Into<Syscall>,
    {
        let result = self.record_or_replay(guest, syscall).await?;
        self.canonicalize_namespace_readlink_result(guest, path, buffer, buffer_len, result)
            .await
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#877)
    /// Preserve Linux readlink errors and canonicalize procfs namespace identities.
    pub async fn handle_readlink<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readlink,
    ) -> Result<i64, Error> {
        let path: PathBuf = call.path().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        let (host_path, other_proc_fd) = if let Some((Some(subject), fd)) = proc_fd_target(&path) {
            let current_pid = guest.inject(syscalls::Getpid::new()).await?;
            (
                host_self_proc_fd_alias(&path, current_pid),
                (current_pid != i64::from(subject)).then_some(OtherProcFdLink { subject, fd }),
            )
        } else {
            (None, None)
        };
        if let Some(host_path) = host_path {
            let bytes = host_path.as_os_str().as_bytes();
            let mut path_buffer = [0_u8; 64];
            path_buffer[..bytes.len()].copy_from_slice(bytes);
            let mut stack = guest.stack().await;
            let path_address = stack.push(path_buffer).cast::<libc::c_char>();
            let stack_guard = stack.commit()?;
            let physical_call = call.with_path(PathPtr::from_ptr(
                path_address.as_raw() as *const libc::c_char
            ));
            let result = self.record_or_replay(guest, physical_call).await?;
            drop(stack_guard);
            return self
                .canonicalize_namespace_readlink_result(
                    guest,
                    path,
                    call.buf(),
                    call.bufsize(),
                    result,
                )
                .await;
        }
        if let Some(link) = other_proc_fd {
            return self.finish_other_proc_fd_readlink(guest, link, call).await;
        }
        self.finish_namespace_readlink(guest, path, call.buf(), call.bufsize(), call)
            .await
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#877)
    /// Preserve Linux readlinkat errors and canonicalize absolute procfs namespace identities.
    pub async fn handle_readlinkat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readlinkat,
    ) -> Result<i64, Error> {
        let path: PathBuf = call.path().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        let observed_path = if path.is_absolute() || call.dirfd() == libc::AT_FDCWD {
            path
        } else {
            guest
                .thread_state()
                .with_detfd(call.dirfd(), |detfd| detfd.path())?
                .map_or(path.clone(), |directory| directory.join(path))
        };
        if let Some((Some(subject), fd)) = proc_fd_target(&observed_path) {
            let current_pid = guest.inject(syscalls::Getpid::new()).await?;
            if current_pid != i64::from(subject) {
                let link = OtherProcFdLink { subject, fd };
                return self
                    .finish_other_proc_fd_readlinkat(guest, link, call)
                    .await;
            }
        }
        self.finish_namespace_readlink(guest, observed_path, call.buf(), call.buf_len(), call)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_process_and_thread_namespace_links() {
        assert_eq!(
            canonical_namespace_target(Path::new("/proc/self/ns/mnt")),
            Some(b"mnt:[4026531841]".as_slice())
        );
        assert_eq!(
            canonical_namespace_target(Path::new("/proc/123/task/456/ns/user")),
            Some(b"user:[4026531837]".as_slice())
        );
        assert_eq!(
            canonical_namespace_target(Path::new("/proc/thread-self/ns/pid_for_children")),
            Some(b"pid:[4026531836]".as_slice())
        );
    }

    #[test]
    fn leaves_non_namespace_and_relative_links_untouched() {
        assert_eq!(
            canonical_namespace_target(Path::new("/proc/self/exe")),
            None
        );
        assert_eq!(canonical_namespace_target(Path::new("/tmp/ns/mnt")), None);
        assert_eq!(
            canonical_namespace_target(Path::new("proc/self/ns/mnt")),
            None
        );
        assert_eq!(
            canonical_namespace_target(Path::new("/proc/self/ns/unknown")),
            None
        );
    }

    #[test]
    fn recognizes_proc_fd_aliases_and_lexical_normalization() {
        for (path, expected) in [
            ("/proc/self/fd/1", (None, 1)),
            ("/proc/thread-self/fd/20", (None, 20)),
            ("/proc/123/fd/7", (Some(123), 7)),
            ("/proc/self/fd/../fd/9", (None, 9)),
            ("/dev/fd/3", (None, 3)),
        ] {
            assert_eq!(proc_fd_target(Path::new(path)), Some(expected), "{path}");
        }
    }

    #[test]
    fn rewrites_only_numeric_virtual_self_proc_fd_aliases() {
        assert_eq!(
            host_self_proc_fd_alias(Path::new("/proc/123/fd/7"), 123),
            Some(PathBuf::from("/proc/self/fd/7"))
        );
        assert_eq!(
            host_self_proc_fd_alias(Path::new("/proc/124/fd/7"), 123),
            None
        );
        assert_eq!(
            host_self_proc_fd_alias(Path::new("/proc/self/fd/7"), 123),
            None
        );
    }

    #[test]
    fn rejects_non_proc_fd_targets() {
        for path in [
            "/proc/self/fd/",
            "/proc/self/fd/stdout",
            "/proc/self/fd/1/status",
            "/proc/not-a-pid/fd/1",
            "/dev/fd/-1",
            "proc/self/fd/1",
        ] {
            assert_eq!(proc_fd_target(Path::new(path)), None, "{path}");
        }
    }

    #[test]
    fn recognizes_only_anonymous_pipe_and_socket_targets() {
        assert_eq!(
            anonymous_proc_fd_identity(b"pipe:[987654321]"),
            Some(AnonymousProcFdIdentity {
                kind: "pipe",
                raw_inode: 987_654_321,
            })
        );
        assert_eq!(
            anonymous_proc_fd_identity(b"socket:[42]"),
            Some(AnonymousProcFdIdentity {
                kind: "socket",
                raw_inode: 42,
            })
        );

        for target in [
            b"pip".as_slice(),
            b"socket:[12345".as_slice(),
            b"/tmp/regular-file".as_slice(),
            b"anon_inode:[eventpoll]".as_slice(),
            b"pipe:[]".as_slice(),
            b"pipe:[12]suffix".as_slice(),
            b"socket:[not-a-number]".as_slice(),
        ] {
            assert_eq!(anonymous_proc_fd_identity(target), None, "{target:?}");
        }
    }

    #[test]
    fn short_buffers_use_scratch_large_enough_for_every_anonymous_target() {
        assert!(!needs_anonymous_proc_fd_scratch(false, 1));
        assert!(!needs_anonymous_proc_fd_scratch(true, 0));
        assert!(needs_anonymous_proc_fd_scratch(true, 1));
        assert!(needs_anonymous_proc_fd_scratch(true, 31));
        assert!(!needs_anonymous_proc_fd_scratch(true, 32));

        let maximum = format!("socket:[{}]", RawInode::MAX);
        assert!(maximum.len() < ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        assert_eq!(
            anonymous_proc_fd_identity(maximum.as_bytes()),
            Some(AnonymousProcFdIdentity {
                kind: "socket",
                raw_inode: RawInode::MAX,
            })
        );
    }

    #[test]
    fn stdio_identity_requires_a_raw_inode_match_and_preserves_alias_precedence() {
        let id = |inode| RawFileId::new(0x7, inode);
        let stdio = [Some(id(11)), Some(id(22)), Some(id(33))];
        assert_eq!(
            deterministic_stdio_inode_for_raw(id(22), &stdio),
            Some(DetInode::mint(1001))
        );
        assert_eq!(deterministic_stdio_inode_for_raw(id(44), &stdio), None);

        let aliased = [None, Some(id(55)), Some(id(55))];
        assert_eq!(
            deterministic_stdio_inode_for_raw(id(55), &aliased),
            Some(DetInode::mint(1001))
        );
        // What the tracer-side backends cache: fstat(0) in every slot.
        let all_stdin = [Some(id(66)); 3];
        assert_eq!(
            deterministic_stdio_inode_for_raw(id(66), &all_stdin),
            Some(DetInode::mint(1000))
        );
    }

    /// Regression test for <https://github.com/rrnewton/hermit/issues/3307>:
    /// an object on another filesystem that happens to share a stdio raw inode
    /// number is not stdio. `/dev/null` is inode 3 on devtmpfs, and the third
    /// file created on a fresh tmpfs is inode 3 too.
    #[test]
    fn stdio_identity_requires_the_same_device() {
        let stdio = [Some(RawFileId::new(0x7, 3)), None, None];
        assert_eq!(
            deterministic_stdio_inode_for_raw(RawFileId::new(0x7, 3), &stdio),
            Some(DetInode::mint(1000))
        );
        assert_eq!(
            deterministic_stdio_inode_for_raw(RawFileId::new(0x2f, 3), &stdio),
            None
        );
    }

    /// pipefs and sockfs are distinct kernel-internal filesystems, and every
    /// pipe (and every socket) reports the one device of its filesystem, which
    /// is what lets an other-process `pipe:[N]` link be keyed like an `fstat`
    /// of the same pipe.
    #[test]
    fn anonymous_object_devices_match_fresh_objects() {
        init_anonymous_object_devices();
        let devices = anonymous_object_devices().expect("probe pipefs and sockfs");
        assert_ne!(devices.pipe, devices.socket);
        assert_ne!(devices.pipe, 0, "no filesystem has device 0");
        assert_ne!(devices.socket, 0, "no filesystem has device 0");
        let (reader, _writer) = std::io::pipe().unwrap();
        let pipe = File::from(OwnedFd::from(reader)).metadata().unwrap();
        assert_eq!(pipe.dev(), devices.pipe);
        let (left, _right) = std::os::unix::net::UnixStream::pair().unwrap();
        let socket = File::from(OwnedFd::from(left)).metadata().unwrap();
        assert_eq!(socket.dev(), devices.socket);

        let link = anonymous_proc_fd_identity(format!("pipe:[{}]", pipe.ino()).as_bytes())
            .expect("pipe link");
        assert_eq!(
            link.raw_file_id(devices),
            RawFileId::new(pipe.dev(), pipe.ino())
        );
        let link = anonymous_proc_fd_identity(format!("socket:[{}]", socket.ino()).as_bytes())
            .expect("socket link");
        assert_eq!(
            link.raw_file_id(devices),
            RawFileId::new(socket.dev(), socket.ino())
        );
    }

    /// A failed or missing construction-time probe is an error for an
    /// unconfirmed link, never a substitute device such as 0, which no
    /// `fstat` of the object reports; a successful probe keys the link on
    /// the probed device.
    #[test]
    fn a_failed_or_missing_device_probe_is_an_error_not_a_substitute_device() {
        let failed = Err("failed to probe the pipefs device: EMFILE".to_owned());
        let error = cached_anonymous_object_devices(Some(&failed))
            .expect_err("a failed probe must not yield devices");
        assert!(error.to_string().contains("EMFILE"), "{error}");
        let error = cached_anonymous_object_devices(None)
            .expect_err("a probe that never ran must not yield devices");
        assert!(error.to_string().contains("not probed"), "{error}");

        let devices = AnonymousObjectDevices {
            pipe: 0x9,
            socket: 0x8,
        };
        let probed = cached_anonymous_object_devices(Some(&Ok(devices))).unwrap();
        assert_eq!(probed, devices);
        let pipe = anonymous_proc_fd_identity(b"pipe:[42]").unwrap();
        let socket = anonymous_proc_fd_identity(b"socket:[43]").unwrap();
        assert_eq!(pipe.raw_file_id(probed), RawFileId::new(0x9, 42));
        assert_eq!(socket.raw_file_id(probed), RawFileId::new(0x8, 43));
    }

    /// A guest-side `stat` of the link is accepted only for the same kind of
    /// object and the same inode as the readlink target it confirms; anything
    /// else means the descriptor changed in between.
    #[test]
    fn link_stat_must_describe_the_object_the_target_names() {
        let pipe = anonymous_proc_fd_identity(b"pipe:[42]").unwrap();
        assert!(pipe.matches_stat(libc::S_IFIFO | 0o600, 42));
        assert!(!pipe.matches_stat(libc::S_IFIFO | 0o600, 43));
        assert!(!pipe.matches_stat(libc::S_IFSOCK | 0o777, 42));
        assert!(!pipe.matches_stat(libc::S_IFREG | 0o600, 42));
        let socket = anonymous_proc_fd_identity(b"socket:[7]").unwrap();
        assert!(socket.matches_stat(libc::S_IFSOCK | 0o777, 7));
        assert!(!socket.matches_stat(libc::S_IFIFO | 0o600, 7));
    }

    #[test]
    fn anonymous_target_rewrite_ignores_raw_inode_width_and_truncates_to_buffer() {
        let short_raw = anonymous_proc_fd_identity(b"pipe:[42]").unwrap();
        let long_raw = anonymous_proc_fd_identity(b"pipe:[987654321]").unwrap();
        let short_rewrite =
            canonical_anonymous_proc_fd_target(&short_raw, DetInode::mint(1001), usize::MAX);
        let long_rewrite =
            canonical_anonymous_proc_fd_target(&long_raw, DetInode::mint(1001), usize::MAX);
        assert_eq!(short_rewrite, b"pipe:[1001]");
        assert_eq!(long_rewrite, short_rewrite);

        assert_eq!(
            canonical_anonymous_proc_fd_target(&long_raw, DetInode::mint(1001), 8),
            b"pipe:[10"
        );
    }

    /// `canonicalize_other_proc_fd_target` against the scripted guest of
    /// `files::inject_fstat_scratch`, whose injected syscalls run in this
    /// process. Each case resolves a link of this process's own descriptor,
    /// as a guest's readlink of another process's link would have returned.
    mod other_proc_fd_target {
        use std::os::fd::AsRawFd;

        use reverie::syscalls::Sysno;

        use super::*;
        use crate::syscalls::files::inject_fstat_scratch::FIRST_SCRIPTED_INODE;
        use crate::syscalls::files::inject_fstat_scratch::Pages;
        use crate::syscalls::files::inject_fstat_scratch::ScriptedGuest;

        /// A fresh pipe's read end, kept open by the caller, and its
        /// `(device, inode)`.
        fn pipe_reader() -> (File, u64, u64) {
            let (reader, _writer) = std::io::pipe().unwrap();
            let reader = File::from(OwnedFd::from(reader));
            let metadata = reader.metadata().unwrap();
            (reader, metadata.dev(), metadata.ino())
        }

        /// The inode the guest's cached stat of its stdin reports: the
        /// tracer's `fstat(0)`, which this test process stands in for.
        fn cached_stdin_inode(guest: &ScriptedGuest) -> u64 {
            guest
                .thread
                .with_detfd(libc::STDIN_FILENO, |detfd| {
                    detfd.stat().map(|stat| stat.inode)
                })
                .unwrap()
                .expect("the scripted guest's stdin has a cached stat")
        }

        /// Rewrite `raw_target` as the target of `/proc/<this
        /// process>/fd/<fd>`, returning the rewritten bytes.
        async fn rewrite(
            tool: &Detcore,
            guest: &mut ScriptedGuest,
            fd: i32,
            raw_target: &[u8],
        ) -> Vec<u8> {
            let mut buffer = vec![0u8; 64];
            let address = AddrMut::<libc::c_char>::from_raw(buffer.as_mut_ptr() as usize);
            let link = OtherProcFdLink {
                subject: std::process::id(),
                fd,
            };
            let written = tool
                .canonicalize_other_proc_fd_target(guest, link, raw_target, address, buffer.len())
                .await
                .unwrap()
                .expect("a pipe target is rewritten");
            buffer.truncate(usize::try_from(written).unwrap());
            buffer
        }

        #[tokio::test]
        async fn with_virtualized_metadata_a_link_is_keyed_on_the_guest_stat_of_it() {
            let (reader, device, inode) = pipe_reader();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            assert!(guest.config.virtualize_metadata);

            let target = format!("pipe:[{inode}]");
            let rewritten = rewrite(&tool, &mut guest, reader.as_raw_fd(), target.as_bytes()).await;

            assert_eq!(guest.injected, [Sysno::newfstatat]);
            assert_eq!(
                guest.fstatat_paths,
                [format!("/proc/{}/fd/{}", std::process::id(), reader.as_raw_fd()).into_bytes()]
            );
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [RawFileId::new(device, inode)],
                "the link must be keyed on the device and inode the guest's stat reports"
            );
            assert_eq!(
                rewritten,
                format!("pipe:[{FIRST_SCRIPTED_INODE}]").into_bytes()
            );
        }

        /// When the guest's `stat` of the link cannot confirm the object --
        /// here the descriptor is closed before the rewrite -- the link is
        /// keyed on the pipefs device probed when the tool was constructed,
        /// which is the device an `fstat` of the pipe reports, and never on
        /// device 0 (round-2 review of
        /// <https://github.com/rrnewton/hermit/pull/3255>).
        #[tokio::test]
        async fn with_virtualized_metadata_an_unconfirmed_link_is_keyed_on_the_probed_device() {
            let (reader, device, inode) = pipe_reader();
            let fd = reader.as_raw_fd();
            drop(reader);
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            assert!(guest.config.virtualize_metadata);
            assert_ne!(device, 0);

            let target = format!("pipe:[{inode}]");
            let rewritten = rewrite(&tool, &mut guest, fd, target.as_bytes()).await;

            assert_eq!(
                guest.injected,
                [Sysno::newfstatat],
                "only the confirming stat reaches the guest; the devices were probed earlier"
            );
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [RawFileId::new(device, inode)],
                "an unconfirmed link must be keyed on the pipefs device and its inode"
            );
            assert_eq!(
                rewritten,
                format!("pipe:[{FIRST_SCRIPTED_INODE}]").into_bytes()
            );
        }

        #[tokio::test]
        async fn without_virtualized_metadata_a_link_is_keyed_on_its_inode_alone() {
            let (reader, _, inode) = pipe_reader();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.config.virtualize_metadata = false;
            assert_ne!(
                inode,
                cached_stdin_inode(&guest),
                "precondition: the pipe must not share the cached stdio inode number"
            );

            let target = format!("pipe:[{inode}]");
            let rewritten = rewrite(&tool, &mut guest, reader.as_raw_fd(), target.as_bytes()).await;

            assert_eq!(
                guest.injected,
                [],
                "record and replay must not stat the link: at replay time it names the \
                 replay host's descriptor, not the recorded one"
            );
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [RawFileId::new(0, inode)],
                "record and replay must key the link on its inode alone, on device 0"
            );
            assert_eq!(
                rewritten,
                format!("pipe:[{FIRST_SCRIPTED_INODE}]").into_bytes()
            );
        }

        #[tokio::test]
        async fn without_virtualized_metadata_a_link_matches_stdio_by_inode_alone() {
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.config.virtualize_metadata = false;
            // The cached stdin stat carries its own device; a pipe link
            // carries none, so only the inode can match.
            let target = format!("pipe:[{}]", cached_stdin_inode(&guest));

            let rewritten = rewrite(&tool, &mut guest, libc::STDIN_FILENO, target.as_bytes()).await;

            assert_eq!(
                guest.injected,
                [],
                "record and replay must not stat the link"
            );
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [],
                "a stdio match must not draw an inode from the pool"
            );
            assert_eq!(
                rewritten,
                format!("pipe:[{}]", deterministic_stdio_inode(0).unwrap()).into_bytes()
            );
        }

        /// Rewrite `raw_target`, truncated to `buffer_len` as the kernel (or
        /// the replayer) would have placed it, as the target of the guest's
        /// OWN `/proc/self/fd/<fd>`, returning the rewritten bytes.
        async fn rewrite_own(
            tool: &Detcore,
            guest: &mut ScriptedGuest,
            fd: i32,
            raw_target: &[u8],
            buffer_len: usize,
        ) -> Vec<u8> {
            let mut buffer = vec![0u8; buffer_len];
            let placed = raw_target.len().min(buffer_len);
            buffer[..placed].copy_from_slice(&raw_target[..placed]);
            let address = AddrMut::<libc::c_char>::from_raw(buffer.as_mut_ptr() as usize);
            let written = tool
                .canonicalize_namespace_readlink_result(
                    guest,
                    PathBuf::from(format!("/proc/self/fd/{fd}")),
                    address,
                    buffer_len,
                    i64::try_from(placed).unwrap(),
                )
                .await
                .unwrap();
            buffer.truncate(usize::try_from(written).unwrap());
            buffer
        }

        /// `hermit record`: a child's readlink of its own pipe and its
        /// readlink of its parent's numeric-pid link to the same pipe name
        /// one deterministic inode, because both aliases are keyed on the
        /// inode alone, on device 0.
        #[tokio::test]
        async fn without_virtualized_metadata_own_and_other_links_name_one_inode() {
            let (reader, _, inode) = pipe_reader();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.config.virtualize_metadata = false;
            assert_ne!(
                inode,
                cached_stdin_inode(&guest),
                "precondition: the pipe must not share the cached stdio inode number"
            );
            let target = format!("pipe:[{inode}]");

            let own =
                rewrite_own(&tool, &mut guest, reader.as_raw_fd(), target.as_bytes(), 64).await;
            let other = rewrite(&tool, &mut guest, reader.as_raw_fd(), target.as_bytes()).await;

            assert_eq!(own, format!("pipe:[{FIRST_SCRIPTED_INODE}]").into_bytes());
            assert_eq!(
                other, own,
                "the two aliases of one pipe must name one deterministic inode"
            );
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [RawFileId::new(0, inode), RawFileId::new(0, inode)],
                "both aliases must be keyed in one identity namespace"
            );
            assert_eq!(
                guest.injected,
                [],
                "a complete target must not be confirmed by an fstat that replay cannot repeat"
            );
        }

        /// `hermit replay`: the replayer hands the guest the RECORDED target,
        /// while its descriptor holds a different object (here a pipe whose
        /// inode is not the recorded one). Both aliases still follow the
        /// recorded inode, so they agree with each other and with the
        /// recording.
        #[tokio::test]
        async fn at_replay_own_and_other_links_follow_the_recorded_target() {
            let (replay_descriptor, _, replay_inode) = pipe_reader();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.config.virtualize_metadata = false;
            let recorded_inode = replay_inode + 1_000_003;
            assert_ne!(recorded_inode, cached_stdin_inode(&guest));
            let recorded_target = format!("pipe:[{recorded_inode}]");

            let own = rewrite_own(
                &tool,
                &mut guest,
                replay_descriptor.as_raw_fd(),
                recorded_target.as_bytes(),
                64,
            )
            .await;
            let other = rewrite(
                &tool,
                &mut guest,
                replay_descriptor.as_raw_fd(),
                recorded_target.as_bytes(),
            )
            .await;

            assert_eq!(own, format!("pipe:[{FIRST_SCRIPTED_INODE}]").into_bytes());
            assert_eq!(other, own);
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [
                    RawFileId::new(0, recorded_inode),
                    RawFileId::new(0, recorded_inode)
                ],
                "replay must key the guest's own link on the recorded inode, not on the \
                 replay descriptor's"
            );
            assert_eq!(guest.injected, []);
        }

        /// With `virtualize_metadata` both aliases are keyed on the object's
        /// device and inode: the guest's own link from an `fstat` of the
        /// descriptor, the other process's from a `stat` of its link.
        #[tokio::test]
        async fn with_virtualized_metadata_own_and_other_links_name_one_inode() {
            let (reader, device, inode) = pipe_reader();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            assert!(guest.config.virtualize_metadata);
            let target = format!("pipe:[{inode}]");

            let own =
                rewrite_own(&tool, &mut guest, reader.as_raw_fd(), target.as_bytes(), 64).await;
            let other = rewrite(&tool, &mut guest, reader.as_raw_fd(), target.as_bytes()).await;

            assert_eq!(own, format!("pipe:[{FIRST_SCRIPTED_INODE}]").into_bytes());
            assert_eq!(other, own);
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [RawFileId::new(device, inode), RawFileId::new(device, inode)]
            );
            assert_eq!(guest.injected, [Sysno::fstat, Sysno::newfstatat]);
        }

        /// A target truncated by a short buffer does not parse, so without
        /// `virtualize_metadata` the guest's own link falls back to an
        /// `fstat` of the descriptor, still keyed on its inode alone on
        /// device 0 -- under `hermit record`, the inode the target names.
        #[tokio::test]
        async fn without_virtualized_metadata_a_truncated_own_link_is_keyed_on_the_fstat_inode() {
            let (reader, _, inode) = pipe_reader();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.config.virtualize_metadata = false;
            assert_ne!(inode, cached_stdin_inode(&guest));
            let target = format!("pipe:[{inode}]");

            let own =
                rewrite_own(&tool, &mut guest, reader.as_raw_fd(), target.as_bytes(), 8).await;

            assert_eq!(
                own,
                format!("pipe:[{FIRST_SCRIPTED_INODE}]").as_bytes()[..8].to_vec()
            );
            assert_eq!(guest.injected, [Sysno::fstat]);
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [RawFileId::new(0, inode)]
            );
        }
    }
}
