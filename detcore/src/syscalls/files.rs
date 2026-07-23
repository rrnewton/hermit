/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! System calls for dealing with the file system.

use std::fs;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use futures::future::BoxFuture;
use nix::fcntl::AtFlags;
use nix::fcntl::OFlag;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::FcntlCmd::*;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::SockFlag;
use reverie::syscalls::StatPtr;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;
use reverie::syscalls::Timespec;
use reverie::syscalls::family::StatFamily;
use tracing::info;
use tracing::trace;
use tracing::warn;

use crate::config::SchedHeuristic;
use crate::dirents::*;
use crate::fd::*;
use crate::procfs::ProcfsFile;
use crate::procfs::ProcfsLookup;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::runqueue::LAST_PRIORITY;
use crate::stat::*;
use crate::tool_global::*;
use crate::tool_local::Detcore;
use crate::types::*;

/// A conversion from SOCK_* flags to O_* flags which makes unsafe (but checked during testing) assumptions.
fn oflag_from_sock_bits(s_bits: i32) -> OFlag {
    // An otherwise unsafe "cast" which leans on the `linux_flags_assumptions` below.
    OFlag::from_bits_truncate(s_bits & (libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK))
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Inject an extra fstat to retrieve file metadata.
    async fn inject_fstat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        raw_fd: RawFd,
    ) -> Result<libc::stat, Errno> {
        info!(
            "Injecting additional fstat to retrieve file metadata on fd {}.",
            raw_fd
        );
        let mut stack = guest.stack().await;
        let statptr: StatPtr = StatPtr(stack.reserve());
        stack.commit()?;

        // NOTE: Must retry the injection here. This could get interrupted and
        // we don't want to rerun the entire syscall handler twice.
        guest
            .inject_with_retry(Syscall::Fstat(
                syscalls::Fstat::new()
                    .with_fd(raw_fd)
                    .with_stat(Some(statptr)),
            ))
            .await?;

        let copied = statptr.read(&guest.memory())?;
        // clear stack memory used for fstat allocation
        guest
            .memory()
            .write_exact(statptr.0.cast(), &[0; std::mem::size_of::<libc::stat>()])?;
        trace!("extra fstat returned inode {}", copied.st_ino);
        Ok(copied)
    }

    // helper function to track a new file descriptor.
    async fn add_fd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        flags: OFlag,
        ty: FdType,
    ) -> Result<(), Errno> {
        let stat = if guest.config().virtualize_metadata {
            Some(self.inject_fstat(guest, fd).await?.into())
        } else {
            None
        };
        guest.thread_state().add_fd(fd, flags, ty, stat)
    }

    pub(crate) async fn release_port_for_open_file<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file_id: OpenFileId,
    ) -> Option<u16> {
        let mytime = guest.thread_state().thread_logical_time.clone();
        let response = guest
            .send_rpc((mytime, GlobalRequest::ReleasePort(open_file_id)))
            .await;
        match response.1 {
            GlobalResponse::ReleasePort(port) => port,
            other => panic!("unexpected release-port response: {other:?}"),
        }
    }

    pub(crate) async fn restore_port_for_open_file<G: Guest<Self>>(
        &self,
        guest: &mut G,
        open_file_id: OpenFileId,
        port: u16,
    ) {
        let mytime = guest.thread_state().thread_logical_time.clone();
        let response = guest
            .send_rpc((mytime, GlobalRequest::AddUsedPort(port, open_file_id)))
            .await;
        match response.1 {
            GlobalResponse::AddUsedPort => {}
            other => panic!("unexpected restore-port response: {other:?}"),
        }
    }

    async fn with_fd_resource<G, F>(
        &self,
        guest: &mut G,
        resource: Option<ResourceID>,
        permission: Permission,
        operation: F,
    ) -> Result<i64, Error>
    where
        G: Guest<Self>,
        F: for<'a> FnOnce(&'a Self, &'a mut G) -> BoxFuture<'a, Result<i64, Error>>,
    {
        if let Some(resource) = resource {
            let request = guest.thread_state().mk_request(resource, permission);
            resource_request(guest, request).await;
        }

        let result = operation(self, guest).await;
        resource_release_all(guest).await;
        result
    }

    fn procfs_lookup_at<G: Guest<Self>>(
        &self,
        guest: &G,
        dirfd: RawFd,
        path: &Path,
    ) -> ProcfsLookup {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            let base = if dirfd == libc::AT_FDCWD {
                format!("/proc/{}/cwd", guest.pid())
            } else {
                format!("/proc/{}/fd/{dirfd}", guest.pid())
            };
            let Ok(base) = fs::read_link(base) else {
                return ProcfsLookup::NotProcfs;
            };
            base.join(path)
        };

        let lexical = ProcfsLookup::from_path(&path);
        if lexical != ProcfsLookup::NotProcfs {
            return lexical;
        }

        // Resolve the parent separately so a final procfs symlink, notably
        // /proc/self/exe, is still recognized as a procfs entry.
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
            && let Ok(parent) = fs::canonicalize(parent)
        {
            let resolved = ProcfsLookup::from_path(&parent.join(name));
            if resolved != ProcfsLookup::NotProcfs {
                return resolved;
            }
        }

        fs::canonicalize(path).map_or(ProcfsLookup::NotProcfs, |path| {
            ProcfsLookup::from_path(&path)
        })
    }

    fn procfs_self_exe_stat(&self, follow_symlink: bool) -> DetStat {
        let epoch = Timespec {
            tv_sec: self.cfg.epoch.timestamp(),
            tv_nsec: self.cfg.epoch.timestamp_subsec_nanos() as i64,
        };
        DetStat {
            mode: if follow_symlink {
                libc::S_IFREG | 0o555
            } else {
                libc::S_IFLNK | 0o777
            },
            dev: 1,
            inode: 10_007,
            size: 0,
            blksize: 4096,
            blocks: 0,
            atime: epoch,
            btime: epoch,
            ctime: epoch,
            mtime: epoch,
            ..DetStat::default()
        }
    }

    fn self_exe_target<G: Guest<Self>>(&self, guest: &G) -> Result<Vec<u8>, Errno> {
        fs::read_link(format!("/proc/{}/exe", guest.pid()))
            .map(|path| path.as_os_str().as_bytes().to_vec())
            .map_err(|_| Errno::ENOENT)
    }

    fn write_self_exe_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        buffer: Option<AddrMut<libc::c_char>>,
        capacity: usize,
    ) -> Result<i64, Error> {
        let target = self.self_exe_target(guest)?;
        let length = target.len().min(capacity);
        if length != 0 {
            let buffer = buffer.ok_or(Errno::EFAULT)?;
            guest
                .memory()
                .write_exact(buffer.cast(), &target[..length])?;
        }
        Ok(length as i64)
    }

    fn procfs_stat(&self, file: ProcfsFile) -> DetStat {
        let epoch = Timespec {
            tv_sec: self.cfg.epoch.timestamp(),
            tv_nsec: self.cfg.epoch.timestamp_subsec_nanos() as i64,
        };
        let contents_len = file.contents().len() as i64;
        DetStat {
            mode: libc::S_IFREG | 0o444,
            dev: 1,
            inode: file.inode(),
            size: contents_len,
            blksize: 4096,
            blocks: (contents_len + 511) / 512,
            atime: epoch,
            btime: epoch,
            ctime: epoch,
            mtime: epoch,
            ..DetStat::default()
        }
    }

    async fn open_procfs_file<G: Guest<Self>>(
        &self,
        guest: &mut G,
        flags: OFlag,
        file: ProcfsFile,
    ) -> Result<i64, Error> {
        const BACKING_NAME: &[u8; 14] = b"hermit-procfs\0";
        const MAX_CONTENT_BYTES: usize = 256;

        if flags.intersects(
            OFlag::O_WRONLY
                | OFlag::O_RDWR
                | OFlag::O_CREAT
                | OFlag::O_TRUNC
                | OFlag::O_APPEND
                | OFlag::O_TMPFILE,
        ) {
            return Err(Errno::EACCES.into());
        }
        if flags.contains(OFlag::O_DIRECTORY) {
            return Err(Errno::ENOTDIR.into());
        }

        let contents = file.contents();
        debug_assert!(contents.len() <= MAX_CONTENT_BYTES);
        let mut data = [0; MAX_CONTENT_BYTES];
        data[..contents.len()].copy_from_slice(contents);
        let mut stack = guest.stack().await;
        let name = stack.push(*BACKING_NAME);
        let data = stack.push(data);
        let name_ptr = PathPtr::from_ptr(name.as_raw() as *const libc::c_char);
        let data_ptr: Addr<u8> = data.cast();
        stack.commit()?;

        let mut memfd_flags = libc::MFD_ALLOW_SEALING;
        if flags.contains(OFlag::O_CLOEXEC) {
            memfd_flags |= libc::MFD_CLOEXEC;
        }
        let fd = guest
            .inject_with_retry(Syscall::MemfdCreate(
                syscalls::MemfdCreate::new()
                    .with_name(name_ptr)
                    .with_flags(memfd_flags),
            ))
            .await? as RawFd;

        let written = guest
            .inject_with_retry(Syscall::Pwrite64(
                syscalls::Pwrite64::new()
                    .with_fd(fd)
                    .with_buf(Some(data_ptr))
                    .with_len(contents.len())
                    .with_offset(0),
            ))
            .await?;
        if written != contents.len() as i64 {
            return Err(Errno::EIO.into());
        }

        let seals =
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        guest
            .inject_with_retry(Syscall::Fcntl(
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(F_ADD_SEALS(seals)),
            ))
            .await?;
        self.add_fd(guest, fd, flags, FdType::Regular).await?;
        Ok(fd as i64)
    }

    async fn finish_open<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
        flags: OFlag,
        result: Result<i64, Errno>,
    ) -> Result<i64, Error> {
        match result {
            Ok(fd) => {
                let fd = fd as RawFd;
                let fd_type = path.to_str().map_or(FdType::Regular, |fname| {
                    if fname == "/dev/random" || fname == "/dev/urandom" {
                        FdType::Rng
                    } else {
                        FdType::Regular
                    }
                });
                self.add_fd(guest, fd, flags, fd_type).await?;
                Ok(fd as i64)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Openat system call.
    pub async fn handle_openat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat,
    ) -> Result<i64, Error> {
        let path = call.path().ok_or(Errno::EFAULT)?;
        let path: PathBuf = path.read(&guest.memory())?;
        let resource = ResourceID::Path(path.clone());
        let request = guest.thread_state().mk_request(resource, Permission::R);
        resource_request(guest, request).await;

        let result = match self.procfs_lookup_at(guest, call.dirfd(), &path) {
            ProcfsLookup::File(file) => self.open_procfs_file(guest, call.flags(), file).await,
            ProcfsLookup::SelfExe | ProcfsLookup::Missing => Err(Errno::ENOENT.into()),
            ProcfsLookup::NotProcfs => {
                let result = self.record_or_replay(guest, Syscall::Openat(call)).await;
                self.finish_open(guest, &path, call.flags(), result).await
            }
        };
        resource_release_all(guest).await;
        result
    }

    async fn validate_procfs_openat2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat2,
    ) -> Result<OFlag, Error> {
        // Preserve Linux's versioned open_how and path-resolution checks before synthesis.
        // The validation descriptor is closed without ever becoming guest-visible.
        let validation_fd = guest.inject_with_retry(Syscall::Openat2(call)).await? as RawFd;
        guest
            .inject_with_retry(Syscall::Close(
                syscalls::Close::new().with_fd(validation_fd),
            ))
            .await?;

        let how = call.how().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        let raw_flags = i32::try_from(how.flags).map_err(|_| Errno::EINVAL)?;
        OFlag::from_bits(raw_flags).ok_or_else(|| Errno::EINVAL.into())
    }

    /// Openat2 system call.
    pub async fn handle_openat2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Openat2,
    ) -> Result<i64, Error> {
        let path = call.path().ok_or(Errno::EFAULT)?;
        let path: PathBuf = path.read(&guest.memory())?;
        let lookup = self.procfs_lookup_at(guest, call.dirfd(), &path);
        let non_procfs_flags = if lookup == ProcfsLookup::NotProcfs {
            let how = call.how().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
            Some(OFlag::from_bits_truncate(how.flags as i32))
        } else {
            None
        };
        let resource = ResourceID::Path(path.clone());
        let request = guest.thread_state().mk_request(resource, Permission::R);
        resource_request(guest, request).await;

        let result = if let Some(flags) = non_procfs_flags {
            let result = self.record_or_replay(guest, call).await;
            self.finish_open(guest, &path, flags, result).await
        } else {
            match self.validate_procfs_openat2(guest, call).await {
                Ok(flags) => match lookup {
                    ProcfsLookup::File(file) => self.open_procfs_file(guest, flags, file).await,
                    ProcfsLookup::SelfExe | ProcfsLookup::Missing => Err(Errno::ENOENT.into()),
                    ProcfsLookup::NotProcfs => unreachable!(),
                },
                Err(error) => Err(error),
            }
        };
        resource_release_all(guest).await;
        result
    }

    /// statfs system call.
    pub async fn handle_statfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Statfs,
    ) -> Result<i64, Error> {
        let path = call.path().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        if self.procfs_lookup_at(guest, libc::AT_FDCWD, &path) != ProcfsLookup::NotProcfs {
            return Err(Errno::ENOENT.into());
        }
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// SYS_close system call.
    pub async fn handle_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Close,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let res = self.record_or_replay(guest, call).await?;
        if let Some(open_file_id) = guest.thread_state_mut().remove_fd(fd) {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        trace!("Closed {}", fd);
        Ok(res)
    }

    /// SYS_read system call (MAYHANG).
    pub async fn handle_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Read,
    ) -> Result<i64, Error> {
        if call.len() == 0 {
            // Zero-count reads only serve to detect errors.
            let res = guest.inject(Syscall::from(call)).await?;
            return Ok(res);
        }

        let (fd_type, resource) = guest
            .thread_state_mut()
            .with_detfd(call.fd(), |detfd| (detfd.ty(), detfd.resource()))?;

        self.with_fd_resource(guest, resource, Permission::R, move |this, guest| {
            Box::pin(async move {
                match fd_type {
                    FdType::Rng => {
                        trace!("Read call RNG fd {}, simulating...", call.fd());
                        let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
                        let n = this.fill_random_bytes(
                            guest,
                            remote_buf,
                            call.len(),
                            "/dev/[u]random",
                        )?;
                        Ok(n as i64)
                    }
                    FdType::Regular => {
                        if guest.config().deterministic_io {
                            this.deterministic_read(guest, call).await
                        } else {
                            Ok(this.record_or_replay(guest, call).await?)
                        }
                    }
                    FdType::Signalfd | FdType::Eventfd | FdType::Timerfd | FdType::Inotify => {
                        trace!(
                            "Possibly blocking read call on notification fd {}, type {:?}",
                            call.fd(),
                            fd_type
                        );
                        this.execute_nonblockable_fd_syscall(guest, call).await
                    }
                    FdType::Memfd | FdType::Pidfd | FdType::Userfaultfd => {
                        trace!("Read call on unusual fd {}, type {:?}", call.fd(), fd_type);
                        Ok(this.record_or_replay(guest, call).await?)
                    }
                    FdType::Socket | FdType::Pipe => {
                        trace!(
                            "Possibly blocking read call on {:?} fd {}",
                            fd_type,
                            call.fd()
                        );
                        this.execute_nonblockable_fd_syscall(guest, call).await
                    }
                }
            })
        })
        .await
    }

    /// SYS_readv system call (MAYHANG).
    pub async fn handle_readv<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readv,
    ) -> Result<i64, Error> {
        let (fd_type, resource) = guest
            .thread_state_mut()
            .with_detfd(call.fd(), |detfd| (detfd.ty(), detfd.resource()))?;

        self.with_fd_resource(guest, resource, Permission::R, move |this, guest| {
            Box::pin(async move {
                match fd_type {
                    FdType::Socket | FdType::Pipe => {
                        this.execute_nonblockable_fd_syscall(guest, call).await
                    }
                    _ => Ok(this.record_or_replay(guest, call).await?),
                }
            })
        })
        .await
    }

    /// SYS_pread64 system call.
    pub async fn handle_pread64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pread64,
    ) -> Result<i64, Error> {
        if call.len() == 0 {
            return Ok(guest.inject(Syscall::from(call)).await?);
        }

        let (fd_type, resource) = guest
            .thread_state_mut()
            .with_detfd(call.fd(), |detfd| (detfd.ty(), detfd.resource()))?;

        self.with_fd_resource(guest, resource, Permission::R, move |this, guest| {
            Box::pin(async move {
                match fd_type {
                    FdType::Rng => {
                        trace!("Pread64 call RNG fd {}, simulating...", call.fd());
                        let remote_buf = call.buf().ok_or(Errno::EFAULT)?;
                        let n = this.fill_random_bytes(
                            guest,
                            remote_buf,
                            call.len(),
                            "/dev/[u]random",
                        )?;
                        Ok(n as i64)
                    }
                    FdType::Regular if guest.config().deterministic_io => {
                        this.deterministic_pread64(guest, call).await
                    }
                    _ => Ok(this.record_or_replay(guest, call).await?),
                }
            })
        })
        .await
    }

    /// Helper for performing a deterministic read that retries until it gets all its
    /// bytes.
    async fn deterministic_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Read,
    ) -> Result<i64, Error> {
        let mut total_read_bytes = 0;
        let mut remaining_buf = call.len();

        trace!(
            "[detcore/det_io]: Requested read buffer size: {:?}",
            remaining_buf
        );

        loop {
            match guest.inject_with_retry(call).await {
                Ok(res) => {
                    remaining_buf -= res as usize;
                    total_read_bytes += res;

                    trace!(
                        "[detcore/det_io]: Remaining read buffer size: {:?}",
                        remaining_buf
                    );

                    if res == 0 || remaining_buf == 0 {
                        break Ok(total_read_bytes);
                    }

                    // Buf is guaranteed to exist as we already issued a syscall.
                    let old_ptr = call.buf().unwrap().as_raw();
                    call = call
                        .with_len(remaining_buf)
                        .with_buf(AddrMut::<u8>::from_raw(old_ptr + res as usize));
                }
                Err(e) => {
                    break Err(e.into());
                }
            }
        }
    }

    /// Perform a positional read until the requested buffer is full or EOF is reached.
    async fn deterministic_pread64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut call: syscalls::Pread64,
    ) -> Result<i64, Error> {
        let mut total_read_bytes = 0;
        let mut remaining_buf = call.len();

        trace!(
            "[detcore/det_io]: Requested pread64 buffer size: {:?}",
            remaining_buf
        );

        loop {
            match guest.inject_with_retry(call).await {
                Ok(res) => {
                    remaining_buf -= res as usize;
                    total_read_bytes += res;

                    trace!(
                        "[detcore/det_io]: Remaining pread64 buffer size: {:?}",
                        remaining_buf
                    );

                    if res == 0 || remaining_buf == 0 {
                        break Ok(total_read_bytes);
                    }

                    let old_ptr = call
                        .buf()
                        .expect("successful pread64 requires a valid guest buffer")
                        .as_raw();
                    let offset = call.offset().checked_add(res).ok_or(Errno::EOVERFLOW)?;
                    call = call
                        .with_len(remaining_buf)
                        .with_buf(AddrMut::<u8>::from_raw(old_ptr + res as usize))
                        .with_offset(offset);
                }
                Err(error) => break Err(error.into()),
            }
        }
    }

    /// SYS_write system call.
    pub async fn handle_write<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Write,
    ) -> Result<i64, Error> {
        let (fd_type, physically_nonblocking, resource, raw_ino) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.physically_nonblocking(),
                    detfd.resource(),
                    detfd.stat().map(|x| x.inode),
                )
            })?;
        // It doesn't matter much where the linearization point for this mtime bump falls:
        if guest.config().virtualize_metadata {
            let r =
                raw_ino.expect("Expect that when virtualize_metadata, DetFd's stat is populated!");
            touch_file(guest, r).await;
        }

        self.with_fd_resource(guest, resource, Permission::W, move |this, guest| {
            Box::pin(async move {
                if physically_nonblocking
                    && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd)
                {
                    this.execute_nonblockable_fd_syscall(guest, call).await
                } else if guest.config().deterministic_io {
                    let mut call = call;
                    let mut total_written_bytes = 0;
                    let mut remaining_buf = call.len();

                    trace!(
                        "[detcore/det_io]: Requested write buffer size: {:?}",
                        remaining_buf
                    );

                    loop {
                        match this.record_or_replay(guest, call).await {
                            Ok(res) => {
                                remaining_buf -= res as usize;
                                total_written_bytes += res;

                                trace!(
                                    "[detcore/det_io]: Remaining write buffer size: {:?}",
                                    remaining_buf
                                );

                                if res == 0 || remaining_buf == 0 {
                                    break Ok(total_written_bytes);
                                }

                                // Buf is guaranteed to exist as we already issued a syscall.
                                let old_ptr = call.buf().unwrap().as_raw();
                                call = call
                                    .with_len(remaining_buf)
                                    .with_buf(Addr::<u8>::from_raw(old_ptr + res as usize));
                            }
                            Err(e) => break Err(e.into()),
                        }
                    }
                } else {
                    Ok(this.record_or_replay(guest, call).await?)
                }
            })
        })
        .await
    }

    /// SYS_writev system call (MAYHANG).
    pub async fn handle_writev<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Writev,
    ) -> Result<i64, Error> {
        let (fd_type, physically_nonblocking, resource, raw_ino) =
            guest.thread_state().with_detfd(call.fd(), |detfd| {
                (
                    detfd.ty(),
                    detfd.physically_nonblocking(),
                    detfd.resource(),
                    detfd.stat().map(|x| x.inode),
                )
            })?;
        if guest.config().virtualize_metadata {
            let inode =
                raw_ino.expect("Expect that when virtualize_metadata, DetFd's stat is populated!");
            touch_file(guest, inode).await;
        }

        self.with_fd_resource(guest, resource, Permission::W, move |this, guest| {
            Box::pin(async move {
                if physically_nonblocking && matches!(fd_type, FdType::Socket | FdType::Pipe) {
                    this.execute_nonblockable_fd_syscall(guest, call).await
                } else {
                    Ok(this.record_or_replay(guest, call).await?)
                }
            })
        })
        .await
    }

    /// SYS_mmap system call.
    pub async fn handle_mmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Mmap,
    ) -> Result<i64, Error> {
        enum SharedBacking {
            Anonymous,
            File {
                object: SharedMemoryObjectId,
                offset: u64,
            },
        }

        let backing = if call.flags().contains(MapFlags::MAP_SHARED) {
            if call.fd() == -1 {
                Some(SharedBacking::Anonymous)
            } else {
                let offset = u64::try_from(call.offset()).map_err(|_| Errno::EINVAL)?;
                guest
                    .thread_state()
                    .with_detfd(call.fd(), |fd| {
                        let object = fd.stat().map_or_else(
                            || SharedMemoryObjectId::OpenFile {
                                id: fd.open_file_id(),
                            },
                            |stat| SharedMemoryObjectId::File {
                                device: stat.dev,
                                inode: stat.inode,
                            },
                        );
                        SharedBacking::File { object, offset }
                    })
                    .ok()
            }
        } else {
            None
        };
        let len = call.len();
        let result = self.record_or_replay(guest, call).await?;
        let start = usize::try_from(result).expect("a successful mmap must return an address");

        guest.thread_state().unmap_memory(start, len);
        match backing {
            Some(SharedBacking::Anonymous) => {
                guest.thread_state().map_shared_anonymous(start, len);
            }
            Some(SharedBacking::File { object, offset }) => {
                guest
                    .thread_state()
                    .map_shared_object(start, len, object, offset);
            }
            None => {}
        }
        Ok(result)
    }

    /// SYS_munmap system call.
    pub async fn handle_munmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Munmap,
    ) -> Result<i64, Error> {
        let start = call.addr().map(Addr::as_raw).unwrap_or(0);
        let len = call.len();
        let result = self.record_or_replay(guest, call).await?;
        guest.thread_state().unmap_memory(start, len);
        Ok(result)
    }

    /// SYS_mremap system call.
    pub async fn handle_mremap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Mremap,
    ) -> Result<i64, Error> {
        let old_start = call.addr().map(AddrMut::as_raw).unwrap_or(0);
        let old_len = call.old_len();
        let new_len = call.new_len();
        let result = self.record_or_replay(guest, call).await?;
        let new_start =
            usize::try_from(result).expect("a successful mremap must return an address");
        guest
            .thread_state()
            .remap_memory(old_start, old_len, new_start, new_len);
        Ok(result)
    }

    // Determinize stat by doing:
    //   - using virtual inode instead of real inodes. The virtual inodes
    //     increase monolitically and won't be re-used (like ext4)
    //   - use logical modtime which could be used by program like GNU make
    //     to determine file changes
    async fn determinize_stat<G, S>(&self, guest: &mut G, stat: S) -> Result<DetStat, Error>
    where
        G: Guest<Self>,
        S: Into<DetStat>,
    {
        let cfg = guest.config().clone();

        let mut stat: DetStat = stat.into();
        let (d_ino, global_mtime) = determinize_inode(guest, stat.inode).await;
        stat.inode = d_ino; // Reveal only the deterministic inode.

        let epoch_tp = Timespec {
            tv_sec: cfg.epoch.timestamp(),
            tv_nsec: cfg.epoch.timestamp_subsec_nanos() as i64,
        };

        let mtime: Timespec = global_mtime.into();
        stat.atime = epoch_tp;
        stat.ctime = epoch_tp;
        stat.btime = epoch_tp;

        stat.mtime = mtime;

        Ok(stat)
    }

    fn stat_procfs_lookup<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: StatFamily,
    ) -> Result<ProcfsLookup, Errno> {
        let (dirfd, path) = match &call {
            StatFamily::Stat(call) => (libc::AT_FDCWD, call.path()),
            StatFamily::Lstat(call) => (libc::AT_FDCWD, call.path()),
            StatFamily::Fstatat(call) => (call.dirfd(), call.path()),
            StatFamily::Fstat(_) => return Ok(ProcfsLookup::NotProcfs),
        };
        let path = path.ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        Ok(self.procfs_lookup_at(guest, dirfd, &path))
    }

    fn stat_follows_symlinks(call: &StatFamily) -> bool {
        match call {
            #[cfg(not(target_arch = "aarch64"))]
            StatFamily::Lstat(_) => false,
            StatFamily::Fstatat(call) => !call.flags().contains(AtFlags::AT_SYMLINK_NOFOLLOW),
            _ => true,
        }
    }

    /// Handles all stat syscalls.
    pub async fn handle_stat_family<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: StatFamily,
    ) -> Result<i64, Error> {
        let follow_symlink = Self::stat_follows_symlinks(&call);
        match self.stat_procfs_lookup(guest, call)? {
            ProcfsLookup::File(file) => {
                let statptr = call.stat().ok_or(Errno::EFAULT)?;
                let stat: libc::stat = self.procfs_stat(file).into();
                guest.memory().write_value(statptr.0, &stat)?;
                return Ok(0);
            }
            ProcfsLookup::SelfExe => {
                let statptr = call.stat().ok_or(Errno::EFAULT)?;
                let stat: libc::stat = self.procfs_self_exe_stat(follow_symlink).into();
                guest.memory().write_value(statptr.0, &stat)?;
                return Ok(0);
            }
            ProcfsLookup::Missing => return Err(Errno::ENOENT.into()),
            ProcfsLookup::NotProcfs => {}
        }

        if guest.config().virtualize_metadata {
            // NB: let kernel handle error codes, it's not easy to do so without
            // kernel because there're many corner cases. i.e.: even access
            // filepath from tracer may cause tracer to hang under certain fuse
            // filesystem (squashfs_ll).
            guest.inject(Syscall::from(call)).await?;
            let statptr = call.stat().ok_or(Errno::EFAULT)?;
            let mut memory = guest.memory();
            let stat = memory.read_value(statptr.0)?;
            let stat = self.determinize_stat(guest, stat).await?;
            memory.write_value(statptr.0, &stat.into())?;
            Ok(0)
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// statx system call
    pub async fn handle_statx<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Statx,
    ) -> Result<i64, Error> {
        let path = call.path().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        match self.procfs_lookup_at(guest, call.dirfd(), &path) {
            ProcfsLookup::File(file) => {
                let statptr = call.statx().ok_or(Errno::EFAULT)?;
                let stat: libc::statx = self.procfs_stat(file).into();
                guest.memory().write_value(statptr.0, &stat)?;
                return Ok(0);
            }
            ProcfsLookup::SelfExe => {
                let statptr = call.statx().ok_or(Errno::EFAULT)?;
                let follow_symlink = !call.flags().contains(AtFlags::AT_SYMLINK_NOFOLLOW);
                let stat: libc::statx = self.procfs_self_exe_stat(follow_symlink).into();
                guest.memory().write_value(statptr.0, &stat)?;
                return Ok(0);
            }
            ProcfsLookup::Missing => return Err(Errno::ENOENT.into()),
            ProcfsLookup::NotProcfs => {}
        }

        if guest.config().virtualize_metadata {
            // NB: let kernel handle error codes, it's not easy to do so without kernel
            // because there're many corner cases. i.e.: even access filepath from tracer
            // may cause tracer to hang under certain fuse filesystem (squashfs_ll).
            guest.inject(call).await?;
            let statptr = call.statx().ok_or(Errno::EFAULT)?;
            let mut memory = guest.memory();
            let stat = memory.read_value(statptr.0)?;
            let stat = self.determinize_stat(guest, stat).await?;
            memory.write_value(statptr.0, &stat.into())?;
            Ok(0)
        } else {
            Ok(self.record_or_replay(guest, call).await?)
        }
    }

    /// readlink system call.
    pub async fn handle_readlink<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readlink,
    ) -> Result<i64, Error> {
        let path = call.path().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        match self.procfs_lookup_at(guest, libc::AT_FDCWD, &path) {
            ProcfsLookup::SelfExe => self.write_self_exe_link(guest, call.buf(), call.bufsize()),
            ProcfsLookup::File(_) => Err(Errno::EINVAL.into()),
            ProcfsLookup::Missing => Err(Errno::ENOENT.into()),
            ProcfsLookup::NotProcfs => Ok(self.record_or_replay(guest, call).await?),
        }
    }

    /// readlinkat system call.
    pub async fn handle_readlinkat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Readlinkat,
    ) -> Result<i64, Error> {
        let path = call.path().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        match self.procfs_lookup_at(guest, call.dirfd(), &path) {
            ProcfsLookup::SelfExe => self.write_self_exe_link(guest, call.buf(), call.buf_len()),
            ProcfsLookup::File(_) => Err(Errno::EINVAL.into()),
            ProcfsLookup::Missing => Err(Errno::ENOENT.into()),
            ProcfsLookup::NotProcfs => Ok(self.record_or_replay(guest, call).await?),
        }
    }

    fn procfs_access_result(lookup: ProcfsLookup, mode: i32) -> Option<Result<i64, Error>> {
        match lookup {
            ProcfsLookup::File(_) if mode & (libc::W_OK | libc::X_OK) != 0 => {
                Some(Err(Errno::EACCES.into()))
            }
            ProcfsLookup::SelfExe if mode & libc::W_OK != 0 => Some(Err(Errno::EACCES.into())),
            ProcfsLookup::SelfExe | ProcfsLookup::File(_) => Some(Ok(0)),
            ProcfsLookup::Missing => Some(Err(Errno::ENOENT.into())),
            ProcfsLookup::NotProcfs => None,
        }
    }

    /// access system call.
    pub async fn handle_access<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Access,
    ) -> Result<i64, Error> {
        let path = call.path().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        if let Some(result) = Self::procfs_access_result(
            self.procfs_lookup_at(guest, libc::AT_FDCWD, &path),
            call.mode().bits() as i32,
        ) {
            return result;
        }
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// faccessat system call.
    pub async fn handle_faccessat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Faccessat,
    ) -> Result<i64, Error> {
        let path = call.path().ok_or(Errno::EFAULT)?.read(&guest.memory())?;
        if let Some(result) = Self::procfs_access_result(
            self.procfs_lookup_at(guest, call.dirfd(), &path),
            call.mode().bits() as i32,
        ) {
            return result;
        }
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// ioctl system call.
    ///
    /// Terminal-control ioctls (the `TC*`/`TIOC*` family that reads or writes
    /// host termios state or the terminal window size) expose nondeterministic,
    /// host-specific terminal configuration. We report `ENOTTY` for these so the
    /// guest behaves as though its descriptors are not attached to a terminal:
    /// the deterministic, non-interactive code path (no colorized output, no
    /// tty-dependent buffering, no window-size probing). This is exactly what the
    /// kernel already returns for a non-tty fd, so it is transparent to callers
    /// such as `isatty(3)`.
    ///
    /// All other ioctls (e.g. `FIONREAD`, `FIONBIO`, `FICLONE`) are passed
    /// through unchanged; record/replay captures their results deterministically.
    pub async fn handle_ioctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Ioctl,
    ) -> Result<i64, Error> {
        use syscalls::ioctl::Request;
        match call.request() {
            Request::TCGETS(_)
            | Request::TCSETS(_)
            | Request::TCSETSW(_)
            | Request::TCSETSF(_)
            | Request::TCGETA(_)
            | Request::TCSETA(_)
            | Request::TCSETAW(_)
            | Request::TCSETAF(_)
            | Request::TIOCGWINSZ(_)
            | Request::TIOCSWINSZ(_)
            | Request::TIOCGPGRP(_)
            | Request::TIOCSPGRP(_) => Err(Errno::ENOTTY.into()),
            _ => self.passthrough(guest, Syscall::Ioctl(call)).await,
        }
    }

    /// faccessat2 system call (represented as a raw syscall by Reverie).
    pub async fn handle_faccessat2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        args: SyscallArgs,
    ) -> Result<i64, Error> {
        const VALID_FLAGS: i32 = libc::AT_EACCESS | libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH;
        let mode = args.arg2 as i32;
        let flags = args.arg3 as i32;
        if mode & !(libc::R_OK | libc::W_OK | libc::X_OK) != 0 || flags & !VALID_FLAGS != 0 {
            return Err(Errno::EINVAL.into());
        }

        if args.arg1 == 0 {
            return Err(Errno::EFAULT.into());
        }
        let path = PathPtr::from_ptr(args.arg1 as *const libc::c_char)
            .ok_or(Errno::EFAULT)?
            .read(&guest.memory())?;
        if let Some(result) =
            Self::procfs_access_result(self.procfs_lookup_at(guest, args.arg0 as i32, &path), mode)
        {
            return result;
        }
        Ok(self
            .record_or_replay(guest, Syscall::Other(Sysno::faccessat2, args))
            .await?)
    }

    /// fcntl system call
    pub async fn handle_fcntl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Fcntl,
    ) -> Result<i64, Error> {
        let fd = call.fd();
        let o_cloexec = match call.cmd() {
            F_DUPFD_CLOEXEC(_) => OFlag::O_CLOEXEC,
            _ => OFlag::empty(),
        };
        match call.cmd() {
            F_GETFL => {
                let physical_flags = self.record_or_replay(guest, call).await?;
                let logical_nonblocking = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.is_nonblocking())?;
                let nonblocking = i64::from(OFlag::O_NONBLOCK.bits());
                if logical_nonblocking {
                    Ok(physical_flags | nonblocking)
                } else {
                    Ok(physical_flags & !nonblocking)
                }
            }
            F_SETFL(flags) => {
                let fd_type = guest.thread_state().with_detfd(fd, |detfd| detfd.ty())?;
                let force_nonblocking = self.cfg.use_nonblocking_sockets()
                    && !self.cfg.recordreplay_modes
                    && matches!(fd_type, FdType::Socket | FdType::Pipe | FdType::Eventfd);
                let physical_flags = if force_nonblocking {
                    flags | OFlag::O_NONBLOCK.bits()
                } else {
                    flags
                };
                let result = self
                    .record_or_replay(guest, call.with_cmd(F_SETFL(physical_flags)))
                    .await?;
                guest.thread_state().with_detfd(fd, |detfd| {
                    // Record the guest's *logical* status flags (derives logical
                    // nonblocking); when we forced O_NONBLOCK physically without the
                    // guest asking, mark the description physically nonblocking too.
                    detfd.set_status_flags(flags);
                    if force_nonblocking {
                        detfd.set_physically_nonblocking();
                    }
                })?;
                Ok(result)
            }
            F_DUPFD(_) | F_DUPFD_CLOEXEC(_) => {
                let newfd = self.record_or_replay(guest, call).await? as RawFd;
                let replaced = guest.thread_state_mut().dup_fd(fd, newfd, o_cloexec)?;
                if let Some(open_file_id) = replaced {
                    self.release_port_for_open_file(guest, open_file_id).await;
                }
                Ok(newfd as i64)
            }
            F_SETFD(flags) => {
                let result = self.record_or_replay(guest, call).await?;
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.set_cloexec(flags & libc::FD_CLOEXEC != 0);
                })?;
                Ok(result)
            }
            _ => {
                trace!(
                    "[detcore-finishme]: fcntl unhandled cases: {:?}",
                    call.cmd()
                );
                Ok(self.record_or_replay(guest, call).await?)
            }
        }
    }

    /// dup system call.
    pub async fn handle_dup<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = self.record_or_replay(guest, call).await? as RawFd;
        let replaced = guest
            .thread_state_mut()
            .dup_fd(old_fd, new_fd, OFlag::empty())?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(new_fd as i64)
    }

    /// dup2 system call.
    pub async fn handle_dup2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup2,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = call.newfd();
        let res = self.record_or_replay(guest, call).await?;
        let replaced = guest
            .thread_state_mut()
            .dup_fd(old_fd, new_fd, OFlag::empty())?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(res)
    }

    /// dup3 system call.
    pub async fn handle_dup3<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Dup3,
    ) -> Result<i64, Errno> {
        let old_fd = call.oldfd();
        let new_fd = call.newfd();
        let flags = call.flags();
        let res = self.record_or_replay(guest, call).await?;
        let replaced = guest.thread_state_mut().dup_fd(old_fd, new_fd, flags)?;
        if let Some(open_file_id) = replaced {
            self.release_port_for_open_file(guest, open_file_id).await;
        }
        Ok(res)
    }

    /// pipe2 system call.
    pub async fn handle_pipe2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Pipe2,
    ) -> Result<i64, Errno> {
        let internally_nonblocking =
            self.cfg.use_nonblocking_sockets() && !self.cfg.recordreplay_modes;
        let injected = if internally_nonblocking {
            call.with_flags(call.flags() | OFlag::O_NONBLOCK)
        } else {
            call
        };
        let res = self.record_or_replay(guest, injected).await?;
        let memory = guest.memory();

        if let Some(pipefd) = call.pipefd() {
            let fds: [i32; 2] = memory.read_value(pipefd)?;
            self.add_fd(guest, fds[0], call.flags(), FdType::Pipe)
                .await?;
            self.add_fd(guest, fds[1], call.flags(), FdType::Pipe)
                .await?;
            if internally_nonblocking {
                self.maybe_set_nonblocking_fd(guest, fds[0]);
                self.maybe_set_nonblocking_fd(guest, fds[1]);
            }
        }

        Ok(res)
    }

    /// utime syscall: update access/modification time on a file
    pub async fn handle_utime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utime,
    ) -> Result<i64, Errno> {
        let mut stack = guest.stack().await;
        let mut memory = guest.memory();
        let tp: AddrMut<[Timespec; 2]> = stack.reserve();

        let tp_val = match call.times() {
            None => {
                let now: Timespec = thread_observe_time(guest).await.into();
                [now, now]
            }
            Some(times) => {
                let utimptr = times;
                let utimbuf = memory.read_value(utimptr)?;
                [
                    Timespec {
                        tv_sec: utimbuf.actime,
                        tv_nsec: 0,
                    },
                    Timespec {
                        tv_sec: utimbuf.modtime,
                        tv_nsec: 0,
                    },
                ]
            }
        };

        memory.write_value(tp, &tp_val)?;
        stack.commit()?;

        let utimensat = syscalls::Utimensat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(call.path())
            .with_times(Some(tp.into()));

        self.handle_utimensat(guest, utimensat).await
    }

    /// utimes syscall
    pub async fn handle_utimes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimes,
    ) -> Result<i64, Errno> {
        let mut memory = guest.memory();

        let tp: AddrMut<[Timespec; 2]> = match call.times() {
            None => {
                let now: Timespec = thread_observe_time(guest).await.into();
                let mut stack = guest.stack().await;
                let tp: AddrMut<[Timespec; 2]> = stack.reserve();
                memory.write_value(tp, &[now, now])?;
                stack.commit()?;
                tp
            }
            Some(times) => {
                // Convert the timeval array to a timespec array.
                let tvs = memory.read_value(times)?;
                let tp: Addr<[Timespec; 2]> = times.cast();

                // Safety: The address could point to read-only memory and the
                // write below could fail.
                let tp = unsafe { tp.into_mut() };

                memory.write_value(tp, &[tvs[0].into(), tvs[1].into()])?;
                tp
            }
        };

        let utimensat = syscalls::Utimensat::new()
            .with_dirfd(libc::AT_FDCWD)
            .with_path(call.filename())
            .with_times(Some(tp.into()));

        self.handle_utimensat(guest, utimensat).await
    }

    /// ustimensat syscall
    pub async fn handle_utimensat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Utimensat,
    ) -> Result<i64, Errno> {
        self.record_or_replay(guest, call).await
    }

    /// socket system call.
    pub async fn handle_socket<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Socket,
    ) -> Result<i64, Error> {
        // The socket syscall itself is not blocking, but we must decide whether to make the socket
        // returned physically nonblocking.
        if !self.cfg.sequentialize_threads || self.cfg.recordreplay_modes {
            // Allow possibly blocking syscall in record mode
            let fd = self.record_or_replay(guest, call).await? as RawFd;
            self.add_fd(
                guest,
                fd,
                OFlag::from_bits_truncate(call.r#type()),
                FdType::Socket,
            )
            .await?;
            Ok(fd as i64)
        } else {
            // Under run mode, force all sockets to be registered to be nonblocking in the OS:
            let call2 = if self.cfg.use_nonblocking_sockets() {
                call.with_type(call.r#type() | libc::SOCK_NONBLOCK)
            } else {
                call
            };
            let fd = self.record_or_replay(guest, call2).await? as RawFd; // Cannot hang.
            self.add_fd(
                guest,
                fd,
                OFlag::from_bits_truncate(
                    call.r#type() & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC),
                ),
                FdType::Socket,
            )
            .await?;
            self.maybe_set_nonblocking_fd(guest, fd);

            Ok(fd as i64)
        }
    }

    /// socketpair system call.
    pub async fn handle_socketpair<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Socketpair,
    ) -> Result<i64, Error> {
        let call2 = if self.cfg.sequentialize_threads && !self.cfg.debug_externalize_sockets {
            call.with_type(call.r#type() | libc::SOCK_NONBLOCK)
        } else {
            call
        };
        let res = self.record_or_replay(guest, call2).await?;
        if let Some(usockvec) = call.usockvec() {
            let memory = guest.memory();
            let fds: [i32; 2] = memory.read_value(usockvec)?;

            // Logical flags are as requested:
            self.add_fd(
                guest,
                fds[0],
                OFlag::from_bits_truncate(
                    call.r#type() & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC),
                ),
                FdType::Socket,
            )
            .await?;
            self.add_fd(
                guest,
                fds[1],
                OFlag::from_bits_truncate(
                    call.r#type() & (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC),
                ),
                FdType::Socket,
            )
            .await?;

            self.maybe_set_nonblocking_fd(guest, fds[0]);
            self.maybe_set_nonblocking_fd(guest, fds[1]);
        }
        Ok(res)
    }

    /// bind system call.
    pub async fn handle_bind<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Bind,
    ) -> Result<i64, Error> {
        // WIP!
        if guest.config().sched_heuristic == SchedHeuristic::ConnectBind {
            trace!("Scheduling heuristic: reprioritizing bind");
            let resource = ResourceID::PriorityChangePoint(
                LAST_PRIORITY,
                guest.thread_state().thread_logical_time.as_nanos(),
            );
            let req = guest.thread_state().mk_request(resource, Permission::W);
            resource_request(guest, req).await;
        }
        let addr = call.umyaddr().ok_or(Errno::EFAULT)?;
        let sock_fd = call.fd();
        let open_file_id = guest
            .thread_state()
            .with_detfd(sock_fd, |detfd| detfd.open_file_id())?;

        let sockaddr_family = guest.memory().read_value(addr.cast::<u16>())?;
        if sockaddr_family == libc::AF_INET as u16 {
            // For IPv4
            let mut sockaddr_in: libc::sockaddr_in = guest
                .memory()
                .read_value(addr.cast::<libc::sockaddr_in>())?;

            let port = sockaddr_in.sin_port.to_be();
            let ipaddr = Ipv4Addr::from(sockaddr_in.sin_addr.s_addr);
            if port != 0 {
                if guest.config().warn_non_zero_binds {
                    warn!(
                        "Analyze Networking: Non-zero port detected: {:?}:{:?}",
                        ipaddr, port
                    );
                }
                let mytime = guest.thread_state().thread_logical_time.clone();
                // Send RPC to make sure already used ports are not used.
                let resp = guest
                    .send_rpc((mytime, GlobalRequest::AddUsedPort(port, open_file_id)))
                    .await;
                match resp.1 {
                    GlobalResponse::AddUsedPort => {
                        trace!("Added to used port {}", port);
                    }
                    _ => unreachable!(),
                }
            } else {
                let mytime = guest.thread_state().thread_logical_time.clone();
                // Request a determinzed port
                let resp = guest
                    .send_rpc((mytime, GlobalRequest::RequestPort(open_file_id)))
                    .await;
                match resp.1 {
                    GlobalResponse::RequestPort(port_assigned) => {
                        sockaddr_in.sin_port = port_assigned.to_be();
                        guest
                            .memory()
                            .write_value(addr.cast::<libc::sockaddr_in>(), &sockaddr_in)?;
                    }
                    GlobalResponse::PortFull => {
                        return Err(reverie::Error::from(nix::errno::Errno::EADDRINUSE));
                    }
                    _ => unreachable!(),
                }
            }
        } else if sockaddr_family == libc::AF_INET6 as u16 {
            // For IPv6
            let mut sockfaddr_in: libc::sockaddr_in6 = guest
                .memory()
                .read_value(addr.cast::<libc::sockaddr_in6>())?;
            let port = sockfaddr_in.sin6_port.to_be();
            let ipaddr = Ipv6Addr::from(sockfaddr_in.sin6_addr.s6_addr);
            if port != 0 {
                if guest.config().warn_non_zero_binds {
                    warn!(
                        "Analyze Networking: Non-zero port detected: {:?}:{:?}",
                        ipaddr, port
                    );
                }
                let mytime = guest.thread_state().thread_logical_time.clone();
                let resp = guest
                    .send_rpc((mytime, GlobalRequest::AddUsedPort(port, open_file_id)))
                    .await;
                match resp.1 {
                    GlobalResponse::AddUsedPort => {
                        trace!("Added to used port {}", port);
                    }
                    _ => unreachable!(),
                }
            } else {
                let mytime = guest.thread_state().thread_logical_time.clone();
                let resp = guest
                    .send_rpc((mytime, GlobalRequest::RequestPort(open_file_id)))
                    .await;
                match resp.1 {
                    GlobalResponse::RequestPort(port_assigned) => {
                        sockfaddr_in.sin6_port = port_assigned.to_be();
                        guest
                            .memory()
                            .write_value(addr.cast::<libc::sockaddr_in6>(), &sockfaddr_in)?;
                        trace!("Port assigned {}", port_assigned)
                    }
                    GlobalResponse::PortFull => {
                        return Err(reverie::Error::from(nix::errno::Errno::EADDRINUSE));
                    }
                    _ => unreachable!(),
                }
            }
        }
        let res = self.record_or_replay(guest, call).await?;

        Ok(res)
    }

    /// socket system call.
    pub async fn handle_eventfd2<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Eventfd2,
    ) -> Result<i64, Error> {
        let internally_nonblocking =
            self.cfg.use_nonblocking_sockets() && !self.cfg.recordreplay_modes;
        let injected = if internally_nonblocking {
            call.with_flags(call.flags() | syscalls::EfdFlags::EFD_NONBLOCK)
        } else {
            call
        };
        let fd = self.record_or_replay(guest, injected).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::EFD_CLOEXEC | libc::EFD_NONBLOCK),
            ),
            FdType::Eventfd,
        )
        .await?;
        if internally_nonblocking {
            self.maybe_set_nonblocking_fd(guest, fd);
        }
        Ok(fd as i64)
    }

    /// signalfd4 system call.
    pub async fn handle_signalfd4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Signalfd4,
    ) -> Result<i64, Error> {
        let signalfd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            signalfd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::SFD_CLOEXEC | libc::SFD_NONBLOCK),
            ),
            FdType::Signalfd,
        )
        .await?;
        Ok(signalfd as i64)
    }

    /// timerfd_create system call.
    pub async fn handle_timerfd_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdCreate,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(
                call.flags().bits() & (libc::TFD_CLOEXEC | libc::TFD_NONBLOCK),
            ),
            FdType::Timerfd,
        )
        .await?;
        Ok(fd as i64)
    }

    /// Serialize a notification descriptor control operation.
    async fn notification_fd_control<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        resource_request(guest, Resources::new(dettid)).await;
        Ok(self.record_or_replay(guest, call).await?)
    }

    /// timerfd_settime system call.
    pub async fn handle_timerfd_settime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdSettime,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// timerfd_gettime system call.
    pub async fn handle_timerfd_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::TimerfdGettime,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// inotify_init1 system call.
    pub async fn handle_inotify_init1<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::InotifyInit1,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.flags().bits() & (libc::IN_CLOEXEC | libc::IN_NONBLOCK)),
            FdType::Inotify,
        )
        .await?;
        Ok(fd as i64)
    }

    /// inotify_add_watch system call.
    pub async fn handle_inotify_add_watch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::InotifyAddWatch,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// inotify_rm_watch system call.
    pub async fn handle_inotify_rm_watch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::InotifyRmWatch,
    ) -> Result<i64, Error> {
        self.notification_fd_control(guest, call.into()).await
    }

    /// memfd_create system call.
    pub async fn handle_memfd_create<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::MemfdCreate,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate((call.flags() & libc::MFD_CLOEXEC) as i32),
            FdType::Memfd,
        )
        .await?;
        Ok(fd as i64)
    }

    /// userfaultfd system call.
    pub async fn handle_userfaultfd<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Userfaultfd,
    ) -> Result<i64, Error> {
        let fd = self.record_or_replay(guest, call).await? as RawFd;
        self.add_fd(
            guest,
            fd,
            OFlag::from_bits_truncate(call.flags()),
            FdType::Userfaultfd,
        )
        .await?;
        Ok(fd as i64)
    }

    /// accept4 system call (MAYHANG).
    ///
    /// Category: External OR Internal IO
    /// ---------------------------------
    /// When do we know?  We only know if an accept4 did an extra-container IO AFTER it returns.
    /// I.e. we could accept a connection from another endpoint in the container, or from the outside,
    /// and we don't know which at the point where `accept4` is called.
    pub async fn handle_accept4<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Accept4,
    ) -> Result<i64, Error> {
        // This option applies both to the socket we're doing the accept call on, and the connection
        // that we return. We don't have any smart detection yet to separate internal/external, so
        // applies to everything.
        let call2 = if self.cfg.use_nonblocking_sockets() {
            // Let the socket returned from accept4 be physically nonblocking:
            call.with_flags(call.flags() | SockFlag::SOCK_NONBLOCK)
        } else {
            call
        };
        // This will do blocking/polling as appropriate based on the fd status:
        let fd = self.execute_nonblockable_fd_syscall(guest, call2).await? as RawFd;

        self.add_fd(
            guest,
            fd,
            // This will specify whether the socket returned is logically non-blocking:
            oflag_from_sock_bits(call.flags().bits()),
            FdType::Socket,
        )
        .await?;

        self.maybe_set_nonblocking_fd(guest, fd);

        Ok(fd as i64)
    }

    /// getdents system call.
    pub async fn handle_getdents<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getdents,
    ) -> Result<i64, Error> {
        if !guest.config().virtualize_metadata {
            return Ok(self.record_or_replay(guest, call).await?);
        }

        let dirent = call.dirent().ok_or(Errno::EFAULT)?;

        let nb = self.record_or_replay(guest, call).await?;
        if nb == 0 {
            return Ok(0);
        }

        let mut dents_bytes = vec![0; nb as usize];
        dents_bytes.reserve_exact(128);

        guest
            .memory()
            .read_exact(dirent.cast(), dents_bytes.as_mut_slice())?;

        let mut dents = unsafe { deserialize_dirents(&dents_bytes) };
        dents.sort();
        for dent in &mut dents {
            let (d_ino, _) = determinize_inode(guest, dent.ino).await;
            dent.ino = d_ino;
        }

        let mut dents_bytes = vec![0; dents_bytes.len()];
        let _ = unsafe { serialize_dirents(&dents, &mut dents_bytes) };

        guest
            .memory()
            .write_exact(dirent.cast(), dents_bytes.as_slice())?;
        Ok(nb)
    }

    /// getdents64 system call.
    pub async fn handle_getdents64<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getdents64,
    ) -> Result<i64, Error> {
        if !guest.config().virtualize_metadata {
            return Ok(self.record_or_replay(guest, call).await?);
        }

        let dirent = call.dirent().ok_or(Errno::EFAULT)?;

        let nb = self.record_or_replay(guest, call).await?;
        if nb == 0 {
            return Ok(0);
        }

        let mut dents_bytes = vec![0; nb as usize];
        dents_bytes.reserve_exact(128);

        guest
            .memory()
            .read_exact(dirent.cast(), dents_bytes.as_mut_slice())?;

        let mut dents = unsafe { deserialize_dirents64(&dents_bytes) };
        dents.sort();
        for dent in &mut dents {
            let (d_ino, _) = determinize_inode(guest, dent.ino).await;
            dent.ino = d_ino;
        }

        let mut dents_bytes = vec![0; dents_bytes.len()];
        let _ = unsafe { serialize_dirents64(&dents, &mut dents_bytes) };

        guest
            .memory()
            .write_exact(dirent.cast(), dents_bytes.as_slice())?;
        Ok(nb)
    }
}

#[cfg(test)]
mod test {
    use nix::fcntl::OFlag;

    /// This is an assumption we're making about flags.  Probably these flags can never be
    /// changed, but let's check just in case.
    #[test]
    fn linux_flags_assumptions() {
        assert_eq!(libc::SOCK_NONBLOCK, OFlag::O_NONBLOCK.bits());
        assert_eq!(libc::SOCK_CLOEXEC, OFlag::O_CLOEXEC.bits());
    }
}
