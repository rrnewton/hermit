/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic views of kernel namespace metadata.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

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
use crate::IdentityLookupRefused;
use crate::config::AnonymousObjectDevices;
use crate::record_or_replay::RecordOrReplay;
use crate::tool_global::determinize_inode;
use crate::tool_local::Detcore;
use crate::types::DetInode;
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
    /// The inode number the link names. Its device is the kernel's pipe or
    /// socket filesystem ([`anonymous_object_device`]).
    raw_inode: u64,
}

/// The [`AnonymousObjectDevices`], read once per process, or why they could
/// not be read. Detcore reads them when it starts in a process
/// ([`crate::Detcore`]'s `Tool::new`), before any guest syscall, so a guest's
/// readlink never depends on whether Detcore's host process can make a pipe
/// or socket at that moment, and keeps the outcome either way: a later call
/// neither retries nor panics.
///
/// Only a Config without `Config::anonymous_object_devices` needs this read.
/// hermit-cli probes in the Hermit process before launch and, when that probe
/// succeeds, passes the devices to every backend but DBT
/// (`prepare_backend_config`); with them, Detcore neither reads here when it
/// starts nor consults this for a link (`resolve_anonymous_object_devices`).
/// Without virtualized metadata nothing reads the devices, so nothing reads
/// here either.
///
/// Why one read is enough: pipefs and sockfs are each a single
/// kernel-internal mount made at boot (`kern_mount` in `init_pipe_fs` in
/// fs/pipe.c and in `sock_init` in net/socket.c). Every pipe and every socket
/// inode is allocated on that superblock, and `stat` reports its `s_dev`
/// untranslated in every mount, network and user namespace.
///
/// ⚠️ THE READ MUST HAPPEN BEFORE THE GUEST CAN RUN CONCURRENTLY WITH IT.
/// detcore-dbt, detcore-sabre, in-guest LiteInst and e9patch's in-guest tool
/// host embed Detcore in the guest process, where the probe's descriptors
/// come from the GUEST's descriptor table. Probing while another guest thread
/// runs -- one blocked in `recvmsg` under `BlockingExternalIO`, say -- would
/// let that thread's next descriptor (an `SCM_RIGHTS` delivery, an `accept`)
/// land on a number that depends on how the two interleaved. Each of those
/// hosts constructs the tool before the process can have a second thread:
/// detcore-dbt at the process's first intercepted system call, before a
/// clone-family call proceeds (detcore-dbt/src/lib.rs, the
/// `runtime.tool.get_or_init` in its system-call handler); detcore-sabre when
/// the plugin initializes (reverie-sabre `reverie_adapter.rs`,
/// `connect_with_root_initializer`); in-guest LiteInst and e9patch at tool
/// installation (`tool_host.rs` in reverie-liteinst and reverie-e9patch),
/// which runs "before application-created threads". That installation is a
/// preloaded library constructor: the dynamic loader, IFUNC resolvers and
/// earlier library constructors have already run, unmonitored, so on these
/// two hosts the probe sees a single-threaded process only if none of that
/// code started a thread. A forked child copies this static already
/// initialized, and `execve` both resets it and unshares the descriptor
/// table, so, subject to that condition, every probe sees a single-threaded
/// process whose descriptors are its own, and it closes what it opened before
/// returning. The tracer-side backends (ptrace, and e9patch preprocessing
/// with ptrace) and KVM probe in the Hermit process, never in a guest
/// descriptor table.
pub(crate) fn anonymous_object_devices() -> &'static Result<AnonymousObjectDevices, String> {
    static DEVICES: std::sync::OnceLock<Result<AnonymousObjectDevices, String>> =
        std::sync::OnceLock::new();
    DEVICES.get_or_init(|| probe_anonymous_object_devices().map_err(|error| error.to_string()))
}

/// Create and `fstat` one pipe and one socket, return the devices they
/// report, and close them. hermit-cli calls this in the Hermit process before
/// launch and passes the result in `Config::anonymous_object_devices`, so that
/// on every backend but DBT no probe runs in a guest's descriptor table. Not
/// on DBT: it receives `None`, which keeps the devices out of its
/// guest-visible config, and Detcore probes in the guest process when it
/// constructs the tool (`anonymous_object_devices`). Every other backend
/// does the same if this call fails in the Hermit process.
pub fn probe_anonymous_object_devices() -> std::io::Result<AnonymousObjectDevices> {
    use std::os::fd::AsFd;
    let device_of = |fd: std::os::fd::BorrowedFd<'_>| {
        nix::sys::stat::fstat(fd)
            .map(|stat| stat.st_dev)
            .map_err(std::io::Error::from)
    };
    let (reader, _writer) = nix::unistd::pipe().map_err(std::io::Error::from)?;
    let (socket, _peer) = std::os::unix::net::UnixDatagram::pair()?;
    Ok(AnonymousObjectDevices {
        pipe: device_of(reader.as_fd())?,
        socket: device_of(socket.as_fd())?,
    })
}

/// The device of a `kind` ("pipe" or "socket") link's inode, from
/// `devices`: the configured ones, or else the ones read at start. When they
/// could not be read, the lookup is refused ([`IdentityLookupRefused`]), not
/// answered with an errno: Detcore cannot give the link a faithful identity,
/// so the refusing process is stopped on every backend, rather than a link
/// identity getting an invented device or a guest getting an error Linux
/// would not return.
fn anonymous_object_device(
    devices: &Result<AnonymousObjectDevices, String>,
    kind: &str,
) -> Result<u64, Error> {
    let devices = devices.as_ref().map_err(|error| {
        IdentityLookupRefused(format!(
            "Detcore could not read the pipe and socket filesystem devices when it started \
             ({error}), so it cannot identify the {kind} a /proc fd link names"
        ))
        .into_error()
    })?;
    Ok(match kind {
        "pipe" => devices.pipe,
        "socket" => devices.socket,
        _ => unreachable!("anonymous_proc_fd_identity names only pipes and sockets, not {kind}"),
    })
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

/// Whether `target` is a complete pipe or socket link target, or one a short
/// buffer may have cut from such a target: it starts with `pipe:[` or
/// `socket:[`, or is itself a prefix of one of them. A path, such as the one
/// a named FIFO's link names, is neither.
fn may_be_anonymous_proc_fd_target(target: &[u8]) -> bool {
    [b"pipe:[".as_slice(), b"socket:[".as_slice()]
        .into_iter()
        .any(|prefix| target.starts_with(prefix) || prefix.starts_with(target))
}

/// The devices to key an unconfirmed link on: those in
/// `Config::anonymous_object_devices` (`configured`), which the Hermit process
/// probed before launch, when the Config carries them, and only otherwise the
/// outcome of this process's own probe (`probed`; in production
/// [`anonymous_object_devices`]). Configured devices win whatever that probe
/// cached, a failure included, and are used without running it, so a
/// readlink never depends on a probe in a guest's descriptor table when the
/// Hermit process supplied the devices.
fn resolve_anonymous_object_devices(
    configured: Option<AnonymousObjectDevices>,
    probed: impl FnOnce() -> Result<AnonymousObjectDevices, String>,
) -> Result<AnonymousObjectDevices, String> {
    match configured {
        Some(devices) => Ok(devices),
        None => probed(),
    }
}

impl AnonymousProcFdIdentity {
    /// Whether a `stat` of the link describes the object this link target
    /// names: the same kind of object and the same inode.
    fn matches_stat(&self, mode: libc::mode_t, inode: u64) -> bool {
        let file_type = match self.kind {
            "pipe" => libc::S_IFIFO,
            "socket" => libc::S_IFSOCK,
            kind => unreachable!("anonymous_proc_fd_identity never yields kind {kind:?}"),
        };
        mode & libc::S_IFMT == file_type && inode == self.raw_inode
    }
}

/// Match a raw identity against the current process's inherited-stdio
/// identities that keep a fixed inode (`Detcore::fixed_stdio_identity_stats`;
/// on every backend but SaBRe, only stdin's object). Callers exclude
/// ordinary replacement objects before filling this array.
///
/// When stdio descriptors alias, the LOWEST matching descriptor wins, as in
/// the maps sanitizer's `stdio_by_raw_file` table: `2>&1`, or one pipe or
/// terminal on all three, gives several descriptors one object, which a link
/// can name with only one inode. For the stdin object the lowest is the one
/// that agrees with the guest's `fstat(0)` and with its own `/proc/self/fd/0`
/// link. Without `virtualize_metadata` every backend but SaBRe keys all three
/// on one stand-in, the `fstat(0)` that `setup_stdio` in `tool_local.rs`
/// caches in the process that runs Detcore -- Hermit's on ptrace and KVM, the
/// guest's on DBT and LiteInst -- so there the stdin object matches every
/// slot.
fn deterministic_stdio_inode_for_raw(
    raw_inode: RawInode,
    stdio_raw_inodes: &[Option<RawInode>; 3],
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
    ) -> Result<Option<RawInode>, Error>
    where
        G: Guest<Self>,
    {
        let path = format!("/proc/{}/fd/{}", link.subject, link.fd);
        let Some(stat) = self.stat_guest_path(guest, path.as_bytes()).await? else {
            return Ok(None);
        };
        Ok(identity
            .matches_stat(stat.st_mode, stat.st_ino)
            .then(|| RawInode::new(stat.st_dev, stat.st_ino)))
    }

    /// Rewrite another process's `pipe:[N]` or `socket:[N]` link target to
    /// name the object's deterministic inode; `None` for any other target.
    ///
    /// The object is keyed on its device and inode, as an `fstat` of it is:
    /// the device comes from the guest's `stat` of the link
    /// (`other_proc_fd_link_identity`), or, when that cannot confirm the
    /// object, from `Config::anonymous_object_devices`, which the Hermit
    /// process probed before launch, or failing that from the devices probed
    /// when the tool was constructed (`anonymous_object_devices`). Only when
    /// there are neither -- no configured devices and a failed construction
    /// probe -- is the readlink refused ([`IdentityLookupRefused`], from
    /// `anonymous_object_device`, which stops the refusing process on every
    /// backend) rather than key the link on a device no `fstat` reports. A
    /// run launched through hermit-cli lacks configured devices when its own
    /// probe failed, and on DBT always: hermit-cli leaves them out of DBT's
    /// `HERMIT_DBT_DETCONFIG`, which the guest can read and which must stay
    /// byte-identical, so every DBT run keys an unconfirmed link on the
    /// devices probed in the guest when the tool was constructed. A DBT guest
    /// that execs with `HERMIT_DBT_DETCONFIG` removed from its environment is
    /// in the same position: the re-injected runtime builds its default
    /// Config, which carries no devices, and probes in the guest.
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
    /// The stdio descriptors are matched on their own identities, and only
    /// where that identity is the stand-in `setup_stdio` caches for all three,
    /// stdin's object (`fixed_stdio_identity_stats`). With
    /// `virtualize_metadata` each identity is the descriptor's own `fstat`,
    /// so a link to stdin's pipe names stdin's fixed inode, and a link to a
    /// stdout pipe that is not stdin's object names the pooled inode that an
    /// alias of stdout above descriptor 2 reports, not descriptor 1's fixed
    /// inode, which no alias reports (round-11 review of
    /// <https://github.com/rrnewton/hermit/pull/3255>).
    ///
    /// ⚠️ WITHOUT IT THE STDIO MATCH IS STILL AGAINST THE RUNNING PROCESS'S
    /// STDIN. Every descriptor's identity is then the stand-in `setup_stdio`
    /// caches, the `fstat(0)` of the process that runs Detcore, taken in each
    /// run, so a recorded link to the recording's stdin object names the
    /// stdio inode only when the replayer's stdin has the same inode number,
    /// and a link to a stdout pipe matches only when it shares stdin's inode
    /// number. This is unchanged from before the pool was keyed on devices.
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
        let stdio_raw_inodes = self.fixed_stdio_identity_stats(guest).await?.map(|stat| {
            stat.map(|stat| {
                if virtualize_metadata {
                    stat.raw_inode()
                } else {
                    RawInode::new(0, stat.inode)
                }
            })
        });
        let raw_file = if virtualize_metadata {
            match self
                .other_proc_fd_link_identity(guest, link, &identity)
                .await?
            {
                Some(raw_file) => raw_file,
                None => {
                    let devices = resolve_anonymous_object_devices(
                        guest.config().anonymous_object_devices,
                        || anonymous_object_devices().clone(),
                    );
                    RawInode::new(
                        anonymous_object_device(&devices, identity.kind)?,
                        identity.raw_inode,
                    )
                }
            }
        } else {
            RawInode::new(0, identity.raw_inode)
        };
        // Every link sends its numbering request, and a stdio match only
        // chooses the rendering afterwards, as `/proc/*/maps` does: whether
        // the raw identity matches stdio is host identity, which must not
        // decide how many inode numbers the call consumes
        // (<https://github.com/rrnewton/hermit/issues/2897>).
        let pooled = determinize_inode(guest, raw_file).await.0;
        let inode =
            deterministic_stdio_inode_for_raw(raw_file, &stdio_raw_inodes).unwrap_or(pooled);
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
    ///
    /// Only a target that is, or may have been cut short from, `pipe:[N]` or
    /// `socket:[N]` is considered (`may_be_anonymous_proc_fd_target`). A named
    /// FIFO is a pipe to `fstat`, but its link names its path, as on Linux,
    /// and is left as the kernel returned it.
    async fn own_proc_fd_link_identity<G>(
        &self,
        guest: &mut G,
        fd: i32,
        observed_target: &[u8],
    ) -> Result<Option<(&'static str, RawInode)>, Error>
    where
        G: Guest<Self>,
    {
        if !may_be_anonymous_proc_fd_target(observed_target) {
            return Ok(None);
        }
        let virtualize_metadata = guest.config().virtualize_metadata;
        if !virtualize_metadata && let Some(identity) = anonymous_proc_fd_identity(observed_target)
        {
            return Ok(Some((identity.kind, RawInode::new(0, identity.raw_inode))));
        }
        let stat = self.inject_fstat(guest, fd).await?;
        let kind = match stat.st_mode & libc::S_IFMT {
            libc::S_IFIFO => "pipe",
            libc::S_IFSOCK => "socket",
            _ => return Ok(None),
        };
        let device = if virtualize_metadata { stat.st_dev } else { 0 };
        Ok(Some((kind, RawInode::new(device, stat.st_ino))))
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

    /// The devices read once are the pipe and socket filesystems' own: a
    /// fresh pipe and socket show the same ones, and the two differ.
    #[test]
    fn anonymous_object_devices_are_the_pipe_and_socket_filesystems() {
        let read = anonymous_object_devices();
        let devices = read.as_ref().unwrap();
        assert_eq!(&probe_anonymous_object_devices().unwrap(), devices);
        assert_ne!(devices.pipe, devices.socket);
        assert_eq!(anonymous_object_device(read, "pipe").unwrap(), devices.pipe);
        assert_eq!(
            anonymous_object_device(read, "socket").unwrap(),
            devices.socket
        );
        // Devices that could not be read at start, as with the process's
        // descriptors exhausted, fail the run as a tool error and never
        // reach the guest as an errno.
        let unread = Err("Too many open files (os error 24)".to_string());
        for kind in ["pipe", "socket"] {
            let error = anonymous_object_device(&unread, kind).unwrap_err();
            assert!(matches!(error, Error::Tool(_)), "{error:?}");
            assert!(error.to_string().contains("os error 24"), "{error}");
        }
    }

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

        let maximum = format!("socket:[{}]", u64::MAX);
        assert!(maximum.len() < ANONYMOUS_PROC_FD_TARGET_CAPACITY);
        assert_eq!(
            anonymous_proc_fd_identity(maximum.as_bytes()),
            Some(AnonymousProcFdIdentity {
                kind: "socket",
                raw_inode: u64::MAX,
            })
        );
    }

    #[test]
    fn stdio_identity_requires_a_raw_inode_match_and_preserves_alias_precedence() {
        let id = |inode| RawInode::new(0x7, inode);
        let stdio = [Some(id(11)), Some(id(22)), Some(id(33))];
        assert_eq!(
            deterministic_stdio_inode_for_raw(id(22), &stdio),
            Some(DetInode::mint(1001))
        );
        assert_eq!(deterministic_stdio_inode_for_raw(id(44), &stdio), None);
        // The same number on another device is another file.
        assert_eq!(
            deterministic_stdio_inode_for_raw(RawInode::new(0x8, 22), &stdio),
            None
        );

        let aliased = [None, Some(id(55)), Some(id(55))];
        assert_eq!(
            deterministic_stdio_inode_for_raw(id(55), &aliased),
            Some(DetInode::mint(1001))
        );
        // What every backend but SaBRe caches: fstat(0) in every slot.
        let all_stdin = [Some(id(66)); 3];
        assert_eq!(
            deterministic_stdio_inode_for_raw(id(66), &all_stdin),
            Some(DetInode::mint(1000))
        );
    }

    /// Devices the Hermit process probed and passed in the Config key an
    /// unconfirmed link whatever this process's own probe gives: a probe that
    /// failed in the guest (`EMFILE` while the guest's descriptor table was
    /// full when Detcore was constructed) no longer turns a readlink that
    /// succeeded into an error, and the probe is not run at all. Without
    /// configured devices the probe's outcome, a failure included, is used
    /// as it is.
    #[test]
    fn configured_devices_key_a_link_even_when_the_construction_probe_failed() {
        let configured = AnonymousObjectDevices {
            pipe: 0xe,
            socket: 0x8,
        };
        let failed = || Err("failed to probe the pipefs device: EMFILE".to_owned());
        assert_eq!(
            resolve_anonymous_object_devices(Some(configured), failed),
            Ok(configured)
        );
        assert_eq!(
            resolve_anonymous_object_devices(Some(configured), || {
                panic!("configured devices must be used without running the probe")
            }),
            Ok(configured)
        );
        let resolved = resolve_anonymous_object_devices(Some(configured), failed);
        assert_eq!(anonymous_object_device(&resolved, "pipe").unwrap(), 0xe);
        assert_eq!(anonymous_object_device(&resolved, "socket").unwrap(), 0x8);

        let unconfigured = resolve_anonymous_object_devices(None, failed);
        assert_eq!(unconfigured, failed());
        let error = anonymous_object_device(&unconfigured, "pipe").unwrap_err();
        assert!(matches!(error, Error::Tool(_)), "{error:?}");
        assert!(error.to_string().contains("EMFILE"), "{error}");
        assert!(
            IdentityLookupRefused::carried_by(&error).is_some(),
            "a failed probe must be a typed refusal, which stops the process: {error:?}"
        );
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
        use std::fs::File;
        use std::os::fd::AsRawFd;
        use std::os::fd::OwnedFd;
        use std::os::unix::fs::MetadataExt;

        use reverie::syscalls::Sysno;

        use super::*;
        use crate::syscalls::files::inject_fstat_scratch::FIRST_SCRIPTED_INODE;
        use crate::syscalls::files::inject_fstat_scratch::Pages;
        use crate::syscalls::files::inject_fstat_scratch::ScriptedGuest;

        /// With `virtualize_metadata`, the first link a process resolves
        /// asks each inherited stdio descriptor for its own identity, one
        /// `fstat` per descriptor, before it can tell whether the link names
        /// a stdio object (`fixed_stdio_identity_stats`, through
        /// `inherited_stdio_identity_stats`; comparing with the cached
        /// stand-in asks nothing). The answers are cached on the
        /// descriptors' open file descriptions, so later links ask nothing
        /// more.
        const STDIO_IDENTITY_FSTATS: [Sysno; 3] = [Sysno::fstat; 3];

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

            assert_eq!(
                guest.injected,
                [STDIO_IDENTITY_FSTATS.as_slice(), &[Sysno::newfstatat]].concat(),
                "the stdio descriptors' own identities, then the stat of the link"
            );
            assert_eq!(
                guest.fstatat_paths,
                [format!("/proc/{}/fd/{}", std::process::id(), reader.as_raw_fd()).into_bytes()]
            );
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [RawInode::new(device, inode)],
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
                [STDIO_IDENTITY_FSTATS.as_slice(), &[Sysno::newfstatat]].concat(),
                "besides the stdio descriptors' own identities, only the confirming stat \
                 reaches the guest; the devices were probed earlier"
            );
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [RawInode::new(device, inode)],
                "an unconfirmed link must be keyed on the pipefs device and its inode"
            );
            assert_eq!(
                rewritten,
                format!("pipe:[{FIRST_SCRIPTED_INODE}]").into_bytes()
            );
        }

        /// The devices the Hermit process passed in the Config are the ones an
        /// unconfirmed link is keyed on, whatever the construction probe in
        /// this address space cached; so a probe that failed in a guest's
        /// full descriptor table can no longer fail the readlink (round-4
        /// review of <https://github.com/rrnewton/hermit/pull/3255>). The
        /// configured devices here differ from the real ones to show which
        /// source the key came from.
        #[tokio::test]
        async fn with_virtualized_metadata_an_unconfirmed_link_is_keyed_on_the_configured_device() {
            let (reader, device, inode) = pipe_reader();
            let fd = reader.as_raw_fd();
            drop(reader);
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            let configured = AnonymousObjectDevices {
                pipe: 0x7777,
                socket: 0x8888,
            };
            assert_ne!(configured.pipe, device);
            guest.config.anonymous_object_devices = Some(configured);

            let target = format!("pipe:[{inode}]");
            let rewritten = rewrite(&tool, &mut guest, fd, target.as_bytes()).await;

            assert_eq!(
                guest.injected,
                [STDIO_IDENTITY_FSTATS.as_slice(), &[Sysno::newfstatat]].concat(),
                "the stdio descriptors' own identities, then the stat of the link"
            );
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [RawInode::new(0x7777, inode)],
                "an unconfirmed link must be keyed on the configured pipefs device"
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
                [RawInode::new(0, inode)],
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
            let stdin_inode = cached_stdin_inode(&guest);
            let target = format!("pipe:[{stdin_inode}]");

            let rewritten = rewrite(&tool, &mut guest, libc::STDIN_FILENO, target.as_bytes()).await;

            assert_eq!(
                guest.injected,
                [],
                "record and replay must not stat the link"
            );
            // Whether the raw identity matches stdio is host identity, so it
            // must not decide how many inode numbers the readlink consumes
            // (<https://github.com/rrnewton/hermit/issues/2897>): the link
            // still sends its numbering request, and the match chooses only
            // the rendered number.
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [RawInode::new(0, stdin_inode)],
                "a stdio match must still send its numbering request"
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
                [RawInode::new(0, inode), RawInode::new(0, inode)],
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
                    RawInode::new(0, recorded_inode),
                    RawInode::new(0, recorded_inode)
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
                [RawInode::new(device, inode), RawInode::new(device, inode)]
            );
            assert_eq!(
                guest.injected,
                [
                    &[Sysno::fstat],
                    STDIO_IDENTITY_FSTATS.as_slice(),
                    &[Sysno::newfstatat]
                ]
                .concat(),
                "the own link's fstat, the stdio descriptors' own identities, then the stat \
                 of the other process's link"
            );
        }

        /// The stdio identities are asked once: a second link resolved by the
        /// same process reaches the guest with only its own stat, because the
        /// first left the answers on the stdio descriptors' open file
        /// descriptions (<https://github.com/rrnewton/hermit/pull/3255>,
        /// round 9).
        #[tokio::test]
        async fn with_virtualized_metadata_stdio_identities_are_asked_once() {
            let (first, _, first_inode) = pipe_reader();
            let (second, _, second_inode) = pipe_reader();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            assert!(guest.config.virtualize_metadata);

            let first_target = format!("pipe:[{first_inode}]");
            rewrite(
                &tool,
                &mut guest,
                first.as_raw_fd(),
                first_target.as_bytes(),
            )
            .await;
            assert_eq!(
                guest.injected,
                [STDIO_IDENTITY_FSTATS.as_slice(), &[Sysno::newfstatat]].concat()
            );
            guest.injected.clear();

            let second_target = format!("pipe:[{second_inode}]");
            rewrite(
                &tool,
                &mut guest,
                second.as_raw_fd(),
                second_target.as_bytes(),
            )
            .await;
            assert_eq!(
                guest.injected,
                [Sysno::newfstatat],
                "a second link must not ask the stdio descriptors again"
            );
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
                [RawInode::new(0, inode)]
            );
        }

        /// A named FIFO is a pipe to `fstat`, but Linux names it by its path
        /// in `/proc/self/fd/<fd>`. Its own link must be left as the kernel
        /// returned it, in both metadata modes, without an `fstat`.
        #[tokio::test]
        async fn a_named_fifos_own_link_keeps_its_path() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("fifo");
            nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
            // O_RDWR does not wait for a peer on Linux.
            let fifo = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let target = path.as_os_str().as_bytes().to_vec();
            for virtualize_metadata in [true, false] {
                let scratch = Pages::map(1, 1);
                let (tool, mut guest) =
                    ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
                guest.answers_determinize_inode = true;
                guest.config.virtualize_metadata = virtualize_metadata;

                let own = rewrite_own(&tool, &mut guest, fifo.as_raw_fd(), &target, 256).await;

                assert_eq!(
                    String::from_utf8_lossy(&own),
                    String::from_utf8_lossy(&target),
                    "virtualize_metadata={virtualize_metadata}: a named FIFO's link names its path"
                );
                assert_eq!(
                    guest.injected,
                    [],
                    "virtualize_metadata={virtualize_metadata}"
                );
                assert_eq!(
                    *guest.determinized.lock().unwrap(),
                    [],
                    "virtualize_metadata={virtualize_metadata}"
                );
            }
        }

        /// `hermit record`: a pipe's own link and the `ino:` line of its
        /// fdinfo name one deterministic inode, because without
        /// `virtualize_metadata` both are keyed on the inode alone, on device
        /// 0. Linux names the same inode in both.
        #[tokio::test]
        async fn without_virtualized_metadata_a_pipes_link_and_fdinfo_name_one_inode() {
            let (reader, device, inode) = pipe_reader();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.config.virtualize_metadata = false;
            assert_ne!(device, 0, "precondition: pipefs is not device 0");
            assert_ne!(inode, cached_stdin_inode(&guest));
            let target = format!("pipe:[{inode}]");

            let link =
                rewrite_own(&tool, &mut guest, reader.as_raw_fd(), target.as_bytes(), 64).await;
            let fdinfo = tool
                .fdinfo_raw_file_id(&mut guest, reader.as_raw_fd(), None, b"")
                .await
                .unwrap();
            let fdinfo_inode = determinize_inode(&mut guest, fdinfo).await.0;

            assert_eq!(
                fdinfo,
                RawInode::new(0, inode),
                "fdinfo must key the pipe as its link does"
            );
            assert_eq!(
                String::from_utf8_lossy(&link),
                format!("pipe:[{}]", fdinfo_inode.as_raw()),
                "the link and the fdinfo ino: line must name one deterministic inode"
            );
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [RawInode::new(0, inode), RawInode::new(0, inode)]
            );
        }

        /// Replay: the link target and the fdinfo bytes are the recording's,
        /// naming the recorded inode R, while the replayer's recreated pipe
        /// has its own live inode P. Both views must key on R, on device 0,
        /// as they did in the recording; keying fdinfo on the live `fstat`
        /// gave the two views two deterministic inodes whenever R != P.
        #[tokio::test]
        async fn at_replay_a_pipes_fdinfo_names_the_recorded_inode_its_link_names() {
            let (reader, device, live_inode) = pipe_reader();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.answers_determinize_inode = true;
            guest.config.virtualize_metadata = false;
            assert_ne!(device, 0, "precondition: pipefs is not device 0");
            let recorded_inode = live_inode + 1_000_003;
            assert_ne!(recorded_inode, cached_stdin_inode(&guest));
            let target = format!("pipe:[{recorded_inode}]");
            let recorded_fdinfo =
                format!("pos:\t0\nflags:\t02000000\nmnt_id:\t15\nino:\t{recorded_inode}\n");

            let link =
                rewrite_own(&tool, &mut guest, reader.as_raw_fd(), target.as_bytes(), 64).await;
            let fdinfo = tool
                .fdinfo_raw_file_id(
                    &mut guest,
                    reader.as_raw_fd(),
                    None,
                    recorded_fdinfo.as_bytes(),
                )
                .await
                .unwrap();
            let fdinfo_inode = determinize_inode(&mut guest, fdinfo).await.0;

            assert_eq!(
                fdinfo,
                RawInode::new(0, recorded_inode),
                "fdinfo must key the pipe on the recorded inode, not the replayer's {live_inode}"
            );
            assert_eq!(
                String::from_utf8_lossy(&link),
                format!("pipe:[{}]", fdinfo_inode.as_raw()),
                "the link and the fdinfo ino: line must name one deterministic inode"
            );
            assert_eq!(
                *guest.determinized.lock().unwrap(),
                [
                    RawInode::new(0, recorded_inode),
                    RawInode::new(0, recorded_inode)
                ]
            );
        }

        /// Control for the two pipe tests above: without `virtualize_metadata` a
        /// regular file's fdinfo stays keyed on its device as well as its
        /// inode, so an equal inode number on another filesystem names
        /// another file (<https://github.com/rrnewton/hermit/issues/3307>).
        #[tokio::test]
        async fn without_virtualized_metadata_fdinfo_keeps_a_regular_files_device() {
            let file = tempfile::tempfile().unwrap();
            let metadata = file.metadata().unwrap();
            let scratch = Pages::map(1, 1);
            let (tool, mut guest) = ScriptedGuest::with_scratch(scratch.address, scratch.len, None);
            guest.config.virtualize_metadata = false;

            let fdinfo = tool
                .fdinfo_raw_file_id(&mut guest, file.as_raw_fd(), None, b"")
                .await
                .unwrap();

            assert_eq!(fdinfo, RawInode::new(metadata.dev(), metadata.ino()));
            assert_eq!(guest.injected, [Sysno::fstat]);
        }
    }
}
