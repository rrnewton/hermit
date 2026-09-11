use super::*;

#[cfg(test)]
mod tests;

pub(crate) struct AliasIdentity {
    pub(crate) root: libc::statx,
    pub(crate) link: libc::statx,
}

pub(crate) struct AliasSource {
    pub(crate) root: File,
    pub(crate) tree: File,
    pub(crate) identity: AliasIdentity,
}

impl AliasSource {
    pub(crate) fn prepare(runtime: RawFd, namespace: (u64, u64)) -> io::Result<Self> {
        let kernel = &Native;
        if identity(&current_namespace(kernel)?) != namespace {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
        if call(
            kernel,
            libc::SYS_fcntl,
            [runtime as usize, libc::F_GET_SEALS as usize, 0, 0, 0, 0],
        )? & i64::from(SEALS)
            != i64::from(SEALS)
        {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        let context = descriptor(unsafe {
            File::from_raw_fd(call(
                kernel,
                libc::SYS_fsopen,
                [c"tmpfs".as_ptr() as usize, 1, 0, 0, 0, 0],
            )? as i32)
        })?;
        call(
            kernel,
            libc::SYS_fsconfig,
            [
                context.as_raw_fd() as usize,
                1,
                c"mode".as_ptr() as usize,
                c"0700".as_ptr() as usize,
                0,
                0,
            ],
        )?;
        call(
            kernel,
            libc::SYS_fsconfig,
            [context.as_raw_fd() as usize, 6, 0, 0, 0, 0],
        )?;
        let root = descriptor(unsafe {
            File::from_raw_fd(call(
                kernel,
                libc::SYS_fsmount,
                [context.as_raw_fd() as usize, 1, 0, 0, 0, 0],
            )? as i32)
        })?;
        let target = CString::new(format!("/proc/self/fd/{runtime}"))?;
        call(
            kernel,
            libc::SYS_symlinkat,
            [
                target.as_ptr() as usize,
                root.as_raw_fd() as usize,
                c"runtime".as_ptr() as usize,
                0,
                0,
                0,
            ],
        )?;
        let link = unsafe {
            File::from_raw_fd(call(
                kernel,
                libc::SYS_openat,
                [
                    root.as_raw_fd() as usize,
                    c"runtime".as_ptr() as usize,
                    (libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) as usize,
                    0,
                    0,
                    0,
                ],
            )? as i32)
        };
        let tree = descriptor(unsafe {
            File::from_raw_fd(call(
                kernel,
                libc::SYS_open_tree,
                [
                    link.as_raw_fd() as usize,
                    c"".as_ptr() as usize,
                    (1 | libc::O_CLOEXEC | libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW)
                        as usize,
                    0,
                    0,
                    0,
                ],
            )? as i32)
        })?;
        let metadata = AliasIdentity {
            root: stat_fd(kernel, root.as_raw_fd())?,
            link: stat_fd(kernel, link.as_raw_fd())?,
        };
        Ok(Self {
            root,
            tree,
            identity: metadata,
        })
    }

    pub(crate) fn verify(&self, kernel: &impl Kernel) -> io::Result<()> {
        let root = stat_fd(kernel, self.root.as_raw_fd())?;
        let link = stat_at(
            kernel,
            self.root.as_raw_fd(),
            c"runtime".as_ptr() as usize,
            libc::AT_SYMLINK_NOFOLLOW,
        )?;
        if identity(&root) != identity(&self.identity.root)
            || root.stx_mnt_id != self.identity.root.stx_mnt_id
            || root.stx_mode & libc::S_IFMT as u16 != libc::S_IFDIR as u16
            || identity(&link) != identity(&self.identity.link)
            || link.stx_mode & libc::S_IFMT as u16 != libc::S_IFLNK as u16
        {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        Ok(())
    }
}
