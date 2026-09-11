use super::Kernel;

pub(in super::super) struct PreExec<'kernel, K> {
    pub kernel: &'kernel K,
    pub stderr: i32,
}

impl<K: Kernel> PreExec<'_, K> {
    pub(in super::super) fn report(&self, message: &'static [u8]) {
        reverie::process::report_pre_exec_failure(self.stderr, message);
    }
}

impl<K: Kernel> Kernel for PreExec<'_, K> {
    fn cleanup_error(&self, error: &std::io::Error) {
        let mut message = [0u8; 96];
        let prefix = b"liteinst pre_exec binding: rollback cleanup errno=";
        message[..prefix.len()].copy_from_slice(prefix);
        let mut digits = [0u8; 10];
        let mut count = 0;
        let mut value = error.raw_os_error().unwrap_or(libc::EIO).unsigned_abs();
        loop {
            digits[count] = b'0' + (value % 10) as u8;
            count += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let mut used = prefix.len();
        while count != 0 {
            count -= 1;
            message[used] = digits[count];
            used += 1;
        }
        message[used] = b'\n';
        reverie::process::report_pre_exec_failure(self.stderr, &message[..used + 1]);
    }
    unsafe fn call(&self, number: i64, args: [usize; 6]) -> i64 {
        let result = unsafe { self.kernel.call(number, args) };
        if let Some(message) = failure(number, args, result) {
            self.report(message);
        }
        result
    }
}

fn failure(number: i64, args: [usize; 6], result: i64) -> Option<&'static [u8]> {
    if !(-4095..0).contains(&result)
        || (number == libc::SYS_fgetxattr
            && matches!(-result as i32, libc::ENODATA | libc::EOPNOTSUPP))
        || (number == libc::SYS_pwrite64 && result == -i64::from(libc::EINTR))
    {
        return None;
    }
    Some(match number {
        libc::SYS_openat => b"liteinst pre_exec binding: openat namespace failed\n",
        libc::SYS_fstatfs => b"liteinst pre_exec binding: fstatfs namespace failed\n",
        libc::SYS_ioctl => b"liteinst pre_exec binding: ioctl NS_GET_NSTYPE failed\n",
        libc::SYS_close => b"liteinst pre_exec binding: close namespace failed\n",
        libc::SYS_statx => b"liteinst pre_exec binding: statx failed\n",
        libc::SYS_faccessat2 => b"liteinst pre_exec binding: faccessat2 X_OK failed\n",
        libc::SYS_fgetxattr => b"liteinst pre_exec binding: fgetxattr security.capability failed\n",
        libc::SYS_mount => b"liteinst pre_exec binding: mount MS_BIND failed\n",
        libc::SYS_move_mount => b"liteinst pre_exec binding: move_mount alias failed\n",
        libc::SYS_readlinkat => b"liteinst pre_exec binding: readlinkat source alias failed\n",
        libc::SYS_umount2 => b"liteinst pre_exec binding: umount2 rollback failed\n",
        libc::SYS_pwrite64 => b"liteinst pre_exec binding: pwrite64 record failed\n",
        libc::SYS_fcntl if args[1] == libc::F_ADD_SEALS as usize => {
            b"liteinst pre_exec binding: fcntl F_ADD_SEALS record failed\n"
        }
        libc::SYS_fcntl if args[1] == libc::F_SETFD as usize => {
            b"liteinst pre_exec binding: fcntl F_SETFD inherited FD failed\n"
        }
        _ => b"liteinst pre_exec binding: unclassified syscall failed\n",
    })
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileExt;

    use super::*;

    struct Fails(i64);
    impl Kernel for Fails {
        unsafe fn call(&self, _: i64, _: [usize; 6]) -> i64 {
            self.0
        }
    }

    #[test]
    fn pre_exec_modeled_syscall_errors_keep_errno_and_name_operation() {
        for (number, command, label) in [
            (libc::SYS_openat, 0, "openat namespace"),
            (libc::SYS_fstatfs, 0, "fstatfs namespace"),
            (libc::SYS_ioctl, 0, "ioctl NS_GET_NSTYPE"),
            (libc::SYS_close, 0, "close namespace"),
            (libc::SYS_statx, 0, "statx"),
            (libc::SYS_faccessat2, 0, "faccessat2 X_OK"),
            (libc::SYS_fgetxattr, 0, "fgetxattr security.capability"),
            (libc::SYS_mount, 0, "mount MS_BIND"),
            (libc::SYS_umount2, 0, "umount2 rollback"),
            (libc::SYS_pwrite64, 0, "pwrite64 record"),
            (
                libc::SYS_fcntl,
                libc::F_ADD_SEALS,
                "fcntl F_ADD_SEALS record",
            ),
            (libc::SYS_fcntl, libc::F_SETFD, "fcntl F_SETFD inherited FD"),
        ] {
            let output = tempfile::tempfile().unwrap();
            let failure = Fails(-i64::from(libc::EINVAL));
            let kernel = PreExec {
                kernel: &failure,
                stderr: output.as_raw_fd(),
            };
            let args = [0, command as usize, 0, 0, 0, 0];
            assert_eq!(
                super::super::call(&kernel, number, args)
                    .unwrap_err()
                    .raw_os_error(),
                Some(libc::EINVAL)
            );
            let mut message = [0; 128];
            let size = output.read_at(&mut message, 0).unwrap();
            assert_eq!(
                &message[..size],
                format!("liteinst pre_exec binding: {label} failed\n").as_bytes()
            );
            let closed_output = PreExec {
                kernel: &failure,
                stderr: -1,
            };
            assert_eq!(unsafe { closed_output.call(number, args) }, failure.0);
        }
    }

    #[test]
    fn pre_exec_success_and_handled_errors_are_silent() {
        for (number, result) in [
            (libc::SYS_mount, 0),
            (libc::SYS_statx, 0),
            (libc::SYS_fgetxattr, -i64::from(libc::ENODATA)),
            (libc::SYS_fgetxattr, -i64::from(libc::EOPNOTSUPP)),
            (libc::SYS_pwrite64, -i64::from(libc::EINTR)),
        ] {
            assert!(failure(number, [0; 6], result).is_none());
        }
    }
}
