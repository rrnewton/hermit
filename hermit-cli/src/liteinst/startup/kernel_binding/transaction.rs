use std::ffi::CString;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::{self};
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;

use super::BINDING_ENV;
use super::EnteredContainer;
use super::PreparedInterpreterLaunch;

pub(super) mod alias;
pub(super) mod pre_exec;
use alias::AliasIdentity;
use alias::AliasSource;

pub(super) const HEADER: usize = 208;
pub(super) const LIMIT: usize = HEADER + 4096;
const SEALS: i32 = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
const STAT_MASK: u32 = libc::STATX_BASIC_STATS | libc::STATX_MNT_ID;

pub(super) trait Kernel {
    unsafe fn call(&self, number: i64, args: [usize; 6]) -> i64;
    fn cleanup_error(&self, _: &io::Error) {}
}

pub(super) struct Native;
impl Kernel for Native {
    unsafe fn call(&self, number: i64, args: [usize; 6]) -> i64 {
        let result: i64;
        unsafe {
            std::arch::asm!("syscall", inlateout("rax") number => result,
                in("rdi") args[0], in("rsi") args[1], in("rdx") args[2],
                in("r10") args[3], in("r8") args[4], in("r9") args[5],
                lateout("rcx") _, lateout("r11") _, options(nostack));
        }
        result
    }
}

fn call(kernel: &impl Kernel, number: i64, args: [usize; 6]) -> io::Result<i64> {
    let result = unsafe { kernel.call(number, args) };
    if (-4095..0).contains(&result) {
        Err(io::Error::from_raw_os_error(-result as i32))
    } else {
        Ok(result)
    }
}

pub(super) fn stat_fd(kernel: &impl Kernel, fd: RawFd) -> io::Result<libc::statx> {
    stat_at(kernel, fd, c"".as_ptr() as usize, libc::AT_EMPTY_PATH)
}

pub(super) fn namespace_fd(kernel: &impl Kernel, fd: RawFd) -> io::Result<libc::statx> {
    let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
    call(
        kernel,
        libc::SYS_fstatfs,
        [fd as usize, &mut filesystem as *mut _ as usize, 0, 0, 0, 0],
    )?;
    if filesystem.f_type != 0x6e736673
        || call(kernel, libc::SYS_ioctl, [fd as usize, 0xb703, 0, 0, 0, 0])?
            != i64::from(libc::CLONE_NEWNS)
    {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    stat_fd(kernel, fd)
}

fn current_namespace(kernel: &impl Kernel) -> io::Result<libc::statx> {
    let fd = call(
        kernel,
        libc::SYS_openat,
        [
            libc::AT_FDCWD as usize,
            c"/proc/thread-self/ns/mnt".as_ptr() as usize,
            (libc::O_RDONLY | libc::O_CLOEXEC) as usize,
            0,
            0,
            0,
        ],
    )? as i32;
    let result = namespace_fd(kernel, fd);
    let closed = call(kernel, libc::SYS_close, [fd as usize, 0, 0, 0, 0, 0]);
    let namespace = result?;
    closed?;
    Ok(namespace)
}

fn stat_at(kernel: &impl Kernel, fd: RawFd, path: usize, flags: i32) -> io::Result<libc::statx> {
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    call(
        kernel,
        libc::SYS_statx,
        [
            fd as usize,
            path,
            flags as usize,
            STAT_MASK as usize,
            &mut stat as *mut _ as usize,
            0,
        ],
    )?;
    if stat.stx_mask & STAT_MASK != STAT_MASK {
        return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
    }
    Ok(stat)
}

pub(super) fn identity(stat: &libc::statx) -> (u64, u64) {
    (
        (u64::from(stat.stx_dev_major) << 32) | u64::from(stat.stx_dev_minor),
        stat.stx_ino,
    )
}

fn unchanged(left: &libc::statx, right: &libc::statx) -> bool {
    identity(left) == identity(right)
        && left.stx_mode == right.stx_mode
        && left.stx_size == right.stx_size
        && left.stx_mtime.tv_sec == right.stx_mtime.tv_sec
        && left.stx_mtime.tv_nsec == right.stx_mtime.tv_nsec
        && left.stx_ctime.tv_sec == right.stx_ctime.tv_sec
        && left.stx_ctime.tv_nsec == right.stx_ctime.tv_nsec
}

pub(super) fn executable(kernel: &impl Kernel, fd: RawFd) -> io::Result<libc::statx> {
    let stat = stat_fd(kernel, fd)?;
    if u32::from(stat.stx_mode) & (libc::S_IFMT | libc::S_ISUID | libc::S_ISGID) != libc::S_IFREG {
        return Err(io::Error::from_raw_os_error(libc::EACCES));
    }
    call(
        kernel,
        libc::SYS_faccessat2,
        [
            fd as usize,
            c"".as_ptr() as usize,
            libc::X_OK as usize,
            (libc::AT_EMPTY_PATH | libc::AT_EACCESS) as usize,
            0,
            0,
        ],
    )?;
    match call(
        kernel,
        libc::SYS_fgetxattr,
        [
            fd as usize,
            c"security.capability".as_ptr() as usize,
            0,
            0,
            0,
            0,
        ],
    ) {
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENODATA | libc::EOPNOTSUPP)) => {}
        Err(error) => return Err(error),
        Ok(_) => return Err(io::Error::from_raw_os_error(libc::EACCES)),
    }
    Ok(stat)
}

pub(super) fn isolate_mounts(kernel: &impl Kernel, owned_namespace: (u64, u64)) -> io::Result<()> {
    if identity(&current_namespace(kernel)?) != owned_namespace {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    call(
        kernel,
        libc::SYS_mount,
        [0, c"/".as_ptr() as usize, 0, (libc::MS_PRIVATE | libc::MS_REC) as usize, 0, 0],
    ).map_err(|error| io::Error::new(
        error.kind(),
        format!("kernel-binding private propagation setup failed: namespace={owned_namespace:?} target=/ flags=MS_PRIVATE|MS_REC: {error}"),
    ))?;
    if identity(&current_namespace(kernel)?) != owned_namespace {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    Ok(())
}

fn mountinfo_error(
    kind: io::ErrorKind,
    reason: &str,
    id: u64,
    row: &str,
    separator: Option<usize>,
) -> io::Error {
    let mut end = row.len().min(512);
    while !row.is_char_boundary(end) {
        end -= 1;
    }
    io::Error::new(
        kind,
        format!(
            "kernel-binding private-mount check: {reason}; covering_mount_id={id} separator={separator:?} row_prefix={:?} omitted_bytes={}",
            &row[..end],
            row.len() - end,
        ),
    )
}

pub(super) fn private_mount(contents: &str, id: u64) -> io::Result<()> {
    let mut found = false;
    for row in contents.lines() {
        let fields = row.split_ascii_whitespace().collect::<Vec<_>>();
        let separator = fields.iter().position(|field| *field == "-");
        if fields.len() < 10 || !separator.is_some_and(|end| end >= 6 && end + 4 == fields.len()) {
            return Err(mountinfo_error(
                io::ErrorKind::InvalidData,
                "malformed mountinfo record",
                id,
                row,
                separator,
            ));
        }
        if fields[0].parse::<u64>().ok() != Some(id) {
            continue;
        }
        if found {
            return Err(mountinfo_error(
                io::ErrorKind::InvalidData,
                "duplicate covering mount",
                id,
                row,
                separator,
            ));
        }
        found = true;
        let end = separator.unwrap();
        for field in &fields[6..end] {
            let tag = field.split(':').next().unwrap();
            let reason = match tag {
                "shared" => "interpreter covering mount has shared propagation",
                "master" => "interpreter covering mount has slave propagation",
                "propagate_from" => "interpreter covering mount has a propagation source",
                "unbindable" => "interpreter covering mount is unbindable, not private",
                _ if *field == "idmapped" => continue,
                _ => "unsupported mountinfo optional field (not classified as propagation)",
            };
            return Err(mountinfo_error(
                io::ErrorKind::Unsupported,
                reason,
                id,
                row,
                separator,
            ));
        }
    }
    if found {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "kernel-binding private-mount check: interpreter covering mount missing; covering_mount_id={id}"
        )))
    }
}

#[cfg(test)]
mod tests;

fn descriptor(file: File) -> io::Result<File> {
    let fd = call(
        &Native,
        libc::SYS_fcntl,
        [
            file.as_raw_fd() as usize,
            libc::F_DUPFD_CLOEXEC as usize,
            3,
            0,
            0,
            0,
        ],
    )?;
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

pub(super) struct NamespaceSnapshot {
    pub(super) namespace: libc::statx,
    paths: [libc::statx; 3],
    original: libc::statx,
}

fn binding_target(image: &crate::liteinst::startup::PinnedImage) -> io::Result<CString> {
    let target = std::fs::canonicalize(image.lookup_path())?;
    Ok(CString::new(target.as_os_str().as_bytes())?)
}

pub(super) struct Transaction {
    record: File,
    original: File,
    target: CString,
    interpreter_path: CString,
    kernel_interpreter_path: CString,
    program_path: CString,
    source_path: CString,
    runtime_path: CString,
    namespace: (u64, u64),
    runtime: RawFd,
    original_image: RawFd,
    source_fds: [RawFd; 3],
    expected: [libc::statx; 3],
}

impl Transaction {
    pub(super) fn prepare(
        launch: &PreparedInterpreterLaunch<()>,
        entered: &EnteredContainer,
        command: &std::process::Command,
    ) -> io::Result<Self> {
        if command
            .get_envs()
            .any(|(name, value)| name == BINDING_ENV && value.is_some())
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "binding discovery already selected",
            ));
        }
        let inputs = launch.original().inputs();
        let source = launch.runtime().image().source();
        let mut expected = Vec::new();
        for image in [inputs.program(), inputs.interpreter(), source] {
            image.revalidate()?;
            expected.push(executable(&Native, image.file().as_raw_fd())?);
        }
        if identity(&expected[0]) == identity(&expected[1])
            || identity(&expected[2]) == identity(&expected[1])
        {
            return Err(io::Error::other(
                "interpreter aliases the selected main or runtime source",
            ));
        }
        let target = binding_target(inputs.interpreter())?;
        if target.as_bytes().is_empty()
            || target.as_bytes().len() > 4095
            || target.as_bytes()[0] != b'/'
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "binding target exceeds absolute path bound",
            ));
        }
        let original = descriptor(
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
                .open(std::path::Path::new(std::ffi::OsStr::from_bytes(
                    target.as_bytes(),
                )))?,
        )?;
        let held = stat_fd(&Native, original.as_raw_fd())?;
        if !unchanged(&held, &expected[1]) || held.stx_mnt_id != expected[1].stx_mnt_id {
            return Err(io::Error::other("held interpreter selection changed"));
        }
        let mountinfo = File::open("/proc/thread-self/mountinfo")?;
        let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
        call(
            &Native,
            libc::SYS_fstatfs,
            [
                mountinfo.as_raw_fd() as usize,
                &mut filesystem as *mut _ as usize,
                0,
                0,
                0,
                0,
            ],
        )?;
        if filesystem.f_type != 0x9fa0 {
            return Err(io::Error::other("mountinfo is not procfs"));
        }
        let mut contents = String::new();
        mountinfo
            .take(1024 * 1024 + 1)
            .read_to_string(&mut contents)?;
        if contents.len() > 1024 * 1024 {
            return Err(io::Error::other("mountinfo exceeds bound"));
        }
        private_mount(&contents, held.stx_mnt_id)?;
        let fd = call(
            &Native,
            libc::SYS_memfd_create,
            [
                c"hermit-interpreter-binding-v2".as_ptr() as usize,
                (libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) as usize,
                0,
                0,
                0,
                0,
            ],
        )?;
        let record = descriptor(unsafe { File::from_raw_fd(fd as i32) })?;
        let runtime = launch.runtime().inherited_fd();
        let original_image = launch.inherited_fd();
        for fd in [runtime, original_image] {
            if call(
                &Native,
                libc::SYS_fcntl,
                [fd as usize, libc::F_GET_SEALS as usize, 0, 0, 0, 0],
            )? & i64::from(SEALS)
                != i64::from(SEALS)
            {
                return Err(io::Error::other("launch image is not sealed"));
            }
        }
        executable(&Native, runtime)?;
        Ok(Self {
            record,
            original,
            target,
            interpreter_path: CString::new(
                inputs.interpreter().lookup_path().as_os_str().as_bytes(),
            )?,
            kernel_interpreter_path: CString::new(
                inputs.interpreter_path().as_os_str().as_bytes(),
            )?,
            program_path: CString::new(inputs.program().lookup_path().as_os_str().as_bytes())?,
            source_path: CString::new(source.lookup_path().as_os_str().as_bytes())?,
            runtime_path: CString::new(format!("/proc/self/fd/{runtime}"))?,
            namespace: entered.identity,
            runtime,
            original_image,
            source_fds: [
                inputs.program().file().as_raw_fd(),
                inputs.interpreter().file().as_raw_fd(),
                source.file().as_raw_fd(),
            ],
            expected: expected
                .try_into()
                .map_err(|_| io::Error::other("image identity count"))?,
        })
    }

    pub(super) fn record_fd(&self) -> RawFd {
        self.record.as_raw_fd()
    }

    pub(super) fn prepare_alias(&self, snapshot: &NamespaceSnapshot) -> io::Result<AliasSource> {
        AliasSource::prepare(self.runtime, identity(&snapshot.namespace))
    }

    pub(super) fn cleanup(
        &self,
        snapshot: &NamespaceSnapshot,
        alias: &AliasSource,
        kernel: &impl Kernel,
    ) -> io::Result<()> {
        if identity(&current_namespace(kernel)?) != identity(&snapshot.namespace) {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
        match stat_at(
            kernel,
            alias.root.as_raw_fd(),
            c"runtime".as_ptr() as usize,
            libc::AT_SYMLINK_NOFOLLOW,
        ) {
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
            Err(error) => return Err(error),
            Ok(_) => alias.verify(kernel)?,
        }
        let mut target = stat_at(
            kernel,
            libc::AT_FDCWD,
            self.target.as_ptr() as usize,
            libc::AT_SYMLINK_NOFOLLOW,
        )?;
        if identity(&target) == identity(&alias.identity.link) {
            let tree = stat_fd(kernel, alias.tree.as_raw_fd())?;
            if target.stx_mnt_id != tree.stx_mnt_id {
                return Err(io::Error::from_raw_os_error(libc::ESTALE));
            }
            call(
                kernel,
                libc::SYS_umount2,
                [
                    self.target.as_ptr() as usize,
                    (libc::MNT_DETACH | libc::UMOUNT_NOFOLLOW) as usize,
                    0,
                    0,
                    0,
                    0,
                ],
            )?;
            target = stat_at(
                kernel,
                libc::AT_FDCWD,
                self.target.as_ptr() as usize,
                libc::AT_SYMLINK_NOFOLLOW,
            )?;
        }
        if !unchanged(&target, &snapshot.original)
            || target.stx_mnt_id != snapshot.original.stx_mnt_id
        {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        call(
            kernel,
            libc::SYS_unlinkat,
            [
                alias.root.as_raw_fd() as usize,
                c"runtime".as_ptr() as usize,
                0,
                0,
                0,
                0,
            ],
        )?;
        match stat_at(
            kernel,
            alias.root.as_raw_fd(),
            c"runtime".as_ptr() as usize,
            libc::AT_SYMLINK_NOFOLLOW,
        ) {
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            Err(error) => Err(error),
            Ok(_) => Err(io::Error::from_raw_os_error(libc::ESTALE)),
        }
    }

    pub(super) fn enter_namespace(&self, kernel: &impl Kernel) -> io::Result<NamespaceSnapshot> {
        let parent = current_namespace(kernel)?;
        if identity(&parent) != self.namespace {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
        for (index, path) in [
            &self.program_path,
            &self.interpreter_path,
            &self.source_path,
        ]
        .iter()
        .enumerate()
        {
            let path_stat = stat_at(kernel, libc::AT_FDCWD, path.as_ptr() as usize, 0)?;
            let held = executable(kernel, self.source_fds[index])?;
            if !unchanged(&path_stat, &self.expected[index])
                || !unchanged(&held, &self.expected[index])
                || path_stat.stx_mnt_id != self.expected[index].stx_mnt_id
                || held.stx_mnt_id != self.expected[index].stx_mnt_id
            {
                return Err(io::Error::from_raw_os_error(libc::ESTALE));
            }
        }
        call(
            kernel,
            libc::SYS_unshare,
            [libc::CLONE_NEWNS as usize, 0, 0, 0, 0, 0],
        )?;
        let namespace = current_namespace(kernel)?;
        if identity(&namespace) == self.namespace {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
        let opened = call(
            kernel,
            libc::SYS_openat,
            [
                libc::AT_FDCWD as usize,
                self.target.as_ptr() as usize,
                (libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) as usize,
                0,
                0,
                0,
            ],
        )? as i32;
        let copied = call(
            kernel,
            libc::SYS_dup3,
            [
                opened as usize,
                self.original.as_raw_fd() as usize,
                libc::O_CLOEXEC as usize,
                0,
                0,
                0,
            ],
        );
        let closed = call(kernel, libc::SYS_close, [opened as usize, 0, 0, 0, 0, 0]);
        copied?;
        closed?;
        let original = stat_fd(kernel, self.original.as_raw_fd())?;
        if !unchanged(&original, &self.expected[1])
            || original.stx_mnt_id == self.expected[1].stx_mnt_id
        {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        let mut paths = self.expected;
        for (index, path) in [
            &self.program_path,
            &self.interpreter_path,
            &self.source_path,
        ]
        .iter()
        .enumerate()
        {
            paths[index] = stat_at(kernel, libc::AT_FDCWD, path.as_ptr() as usize, 0)?;
            if !unchanged(&paths[index], &self.expected[index]) {
                return Err(io::Error::from_raw_os_error(libc::ESTALE));
            }
        }
        Ok(NamespaceSnapshot {
            namespace,
            paths,
            original,
        })
    }

    pub(super) fn bind(
        &self,
        snapshot: &NamespaceSnapshot,
        alias: &AliasSource,
        kernel: &impl Kernel,
    ) -> io::Result<()> {
        let namespace = current_namespace(kernel)?;
        if identity(&namespace) != identity(&snapshot.namespace) {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
        for (index, path) in [
            &self.program_path,
            &self.interpreter_path,
            &self.source_path,
        ]
        .iter()
        .enumerate()
        {
            let path_stat = stat_at(kernel, libc::AT_FDCWD, path.as_ptr() as usize, 0)?;
            let held = executable(kernel, self.source_fds[index])?;
            if !unchanged(&path_stat, &snapshot.paths[index])
                || path_stat.stx_mnt_id != snapshot.paths[index].stx_mnt_id
                || !unchanged(&held, &self.expected[index])
                || held.stx_mnt_id != self.expected[index].stx_mnt_id
            {
                return Err(io::Error::from_raw_os_error(libc::ESTALE));
            }
        }
        let original = stat_fd(kernel, self.original.as_raw_fd())?;
        let target = stat_at(
            kernel,
            libc::AT_FDCWD,
            self.target.as_ptr() as usize,
            libc::AT_SYMLINK_NOFOLLOW,
        )?;
        if !unchanged(&original, &snapshot.original)
            || original.stx_mnt_id != snapshot.original.stx_mnt_id
            || !unchanged(&target, &original)
            || target.stx_mnt_id != original.stx_mnt_id
        {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        let runtime = stat_fd(kernel, self.runtime)?;
        alias.verify(kernel)?;
        let mut link = [0u8; 64];
        let count = call(
            kernel,
            libc::SYS_readlinkat,
            [
                alias.root.as_raw_fd() as usize,
                c"runtime".as_ptr() as usize,
                link.as_mut_ptr() as usize,
                link.len(),
                0,
                0,
            ],
        )? as usize;
        if count > link.len() || &link[..count] != self.runtime_path.as_bytes() {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        call(
            kernel,
            libc::SYS_move_mount,
            [
                alias.tree.as_raw_fd() as usize,
                c"".as_ptr() as usize,
                libc::AT_FDCWD as usize,
                self.target.as_ptr() as usize,
                4,
                0,
            ],
        )?;
        let result = self.finish_binding(kernel, &namespace, &runtime, &original, alias);
        if result.is_err() {
            let cleanup = call(
                kernel,
                libc::SYS_umount2,
                [
                    self.target.as_ptr() as usize,
                    (libc::MNT_DETACH | libc::UMOUNT_NOFOLLOW) as usize,
                    0,
                    0,
                    0,
                    0,
                ],
            );
            if let Err(error) = cleanup {
                kernel.cleanup_error(&error);
            }
        }
        result
    }

    fn finish_binding(
        &self,
        kernel: &impl Kernel,
        namespace: &libc::statx,
        runtime: &libc::statx,
        original: &libc::statx,
        alias: &AliasSource,
    ) -> io::Result<()> {
        let bound = stat_at(
            kernel,
            libc::AT_FDCWD,
            self.target.as_ptr() as usize,
            libc::AT_SYMLINK_NOFOLLOW,
        )?;
        let tree = stat_fd(kernel, alias.tree.as_raw_fd())?;
        if identity(&bound) != identity(&alias.identity.link)
            || bound.stx_mode & libc::S_IFMT as u16 != libc::S_IFLNK as u16
            || bound.stx_mnt_id == original.stx_mnt_id
            || bound.stx_mnt_id != tree.stx_mnt_id
        {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        let selected = stat_at(
            kernel,
            libc::AT_FDCWD,
            self.kernel_interpreter_path.as_ptr() as usize,
            0,
        )?;
        if identity(&selected) != identity(runtime) || selected.stx_mnt_id != runtime.stx_mnt_id {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        let mut record = [0u8; LIMIT];
        let used = encode(
            &mut record,
            [
                self.runtime,
                self.original_image,
                self.original.as_raw_fd(),
                self.record_fd(),
                alias.root.as_raw_fd(),
            ],
            [identity(namespace), self.namespace],
            runtime,
            original,
            &AliasIdentity {
                root: alias.identity.root,
                link: bound,
            },
            self.target.as_bytes(),
        )?;
        let mut written = 0;
        for _ in 0..32 {
            if written == used {
                break;
            }
            match call(
                kernel,
                libc::SYS_pwrite64,
                [
                    self.record_fd() as usize,
                    record[written..].as_ptr() as usize,
                    used - written,
                    written,
                    0,
                    0,
                ],
            ) {
                Ok(count) if count > 0 && count as usize <= used - written => {
                    written += count as usize
                }
                Err(error) if error.raw_os_error() == Some(libc::EINTR) => {}
                Err(error) => return Err(error),
                _ => return Err(io::Error::from_raw_os_error(libc::EIO)),
            }
        }
        if written != used {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        call(
            kernel,
            libc::SYS_fcntl,
            [
                self.record_fd() as usize,
                libc::F_ADD_SEALS as usize,
                SEALS as usize,
                0,
                0,
                0,
            ],
        )?;
        for fd in [
            self.runtime,
            self.original_image,
            self.original.as_raw_fd(),
            self.record_fd(),
            alias.root.as_raw_fd(),
        ] {
            call(
                kernel,
                libc::SYS_fcntl,
                [fd as usize, libc::F_SETFD as usize, 0, 0, 0, 0],
            )?;
        }
        Ok(())
    }
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

pub(super) fn encode(
    bytes: &mut [u8; LIMIT],
    fds: [i32; 5],
    namespaces: [(u64, u64); 2],
    runtime: &libc::statx,
    original: &libc::statx,
    alias: &AliasIdentity,
    target: &[u8],
) -> io::Result<usize> {
    let [namespace, parent] = namespaces;
    let bound = alias.link.stx_mnt_id;
    if target.is_empty()
        || target.len() > 4095
        || target[0] != b'/'
        || target.contains(&0)
        || namespace == parent
        || bound == 0
        || original.stx_mnt_id == 0
        || bound == original.stx_mnt_id
        || alias.link.stx_mode & libc::S_IFMT as u16 != libc::S_IFLNK as u16
        || alias.root.stx_mode & libc::S_IFMT as u16 != libc::S_IFDIR as u16
        || fds
            .iter()
            .enumerate()
            .any(|(index, fd)| *fd < 3 || fds[..index].contains(fd))
    {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    bytes.fill(0);
    bytes[..8].copy_from_slice(b"HLBIND02");
    let used = HEADER + target.len() + 1;
    put32(bytes, 8, 2);
    put32(bytes, 12, used as u32);
    for (index, fd) in fds[..4].iter().enumerate() {
        put32(bytes, 16 + index * 4, *fd as u32);
    }
    put64(bytes, 32, namespace.0);
    put64(bytes, 40, namespace.1);
    put32(bytes, 48, runtime.stx_dev_major);
    put32(bytes, 52, runtime.stx_dev_minor);
    put64(bytes, 56, runtime.stx_ino);
    put32(bytes, 64, original.stx_dev_major);
    put32(bytes, 68, original.stx_dev_minor);
    put64(bytes, 72, original.stx_ino);
    put32(bytes, 80, u32::from(original.stx_mode));
    put32(bytes, 84, target.len() as u32);
    put64(bytes, 88, original.stx_mnt_id);
    put64(bytes, 96, bound);
    put64(bytes, 104, original.stx_size);
    put64(bytes, 112, original.stx_mtime.tv_sec as u64);
    put32(bytes, 120, original.stx_mtime.tv_nsec);
    put32(bytes, 124, original.stx_ctime.tv_nsec);
    put64(bytes, 128, original.stx_ctime.tv_sec as u64);
    put64(bytes, 136, parent.0);
    put64(bytes, 144, parent.1);
    put64(bytes, 152, runtime.stx_mnt_id);
    put64(bytes, 160, identity(&alias.link).0);
    put64(bytes, 168, alias.link.stx_ino);
    put32(bytes, 176, fds[4] as u32);
    put64(bytes, 184, identity(&alias.root).0);
    put64(bytes, 192, alias.root.stx_ino);
    put64(bytes, 200, alias.root.stx_mnt_id);
    bytes[HEADER..HEADER + target.len()].copy_from_slice(target);
    Ok(used)
}
