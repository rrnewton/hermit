/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */

//! Common explicit service-launch policy, not a service ownership registry.
//! The caller retains its unit identity, wrapper child, private channels, actual
//! helper pidfd and recovery resources before any subsequent fallible operation.

use std::ffi::OsString;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

/// Fixed supported privilege launcher; callers never substitute a shell.
pub const CAPABILITY_SUDO: &str = "/usr/bin/sudo";
/// Fixed clean environment, also usable by a prebuilt raw-exec fork split.
pub const CAPABILITY_ENVIRONMENT: &[(&str, &str)] = &[
    ("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"),
    ("LANG", "C"),
    ("LC_ALL", "C"),
];

/// Separate lifecycle owners share limits and loader hygiene, not authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityServiceKind {
    /// Readonly accepted TCP observer, started after clone before guest start.
    Accepted,
    /// Unix policy keeper, started and armed before the container clone.
    UnixGuard,
}

/// Immutable launch inputs. Construction alone grants no guest capability.
#[derive(Debug)]
pub struct CapabilityUnitLaunch<'a> {
    /// Exact service purpose and fixed unit-name prefix.
    pub kind: CapabilityServiceKind,
    /// Run-owned unit name, containing the complete 128-bit incarnation.
    pub unit: &'a str,
    /// Real maintained helper executable, authenticated by the owning caller.
    pub executable: &'a Path,
    /// Data arguments after `--`; never shell-evaluated.
    pub arguments: &'a [OsString],
    /// Existing runner's wall bound, never a new independent guest deadline.
    pub maximum_seconds: u32,
    /// Unix keeper only: exact owned bpffs and recovery directories. These are
    /// mount-policy exceptions, not evidence that a pathname still owns an FD.
    pub writable_directories: &'a [PathBuf],
}

impl CapabilityUnitLaunch<'_> {
    /// Validate and materialize argv before a fork-safe raw exec stub. This is
    /// the same policy used by `command`; it does not launch or transfer owners.
    pub fn arguments(&self) -> io::Result<Vec<OsString>> {
        let prefix = match self.kind {
            CapabilityServiceKind::Accepted => "hermit-accepted-",
            CapabilityServiceKind::UnixGuard => "hermit-unix-",
        };
        let run = self
            .unit
            .strip_prefix(prefix)
            .and_then(|name| name.strip_suffix(".service"))
            .ok_or_else(|| io::Error::other("capability unit purpose/name mismatch"))?;
        if run.len() != 32
            || run.bytes().all(|byte| byte == b'0')
            || !run
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !self.executable.is_absolute()
            || self.maximum_seconds == 0
        {
            return Err(io::Error::other(
                "invalid capability unit identity, executable, or bound",
            ));
        }
        match self.kind {
            CapabilityServiceKind::Accepted if !self.writable_directories.is_empty() => {
                return Err(io::Error::other(
                    "readonly accepted provider has writable directory exception",
                ));
            }
            CapabilityServiceKind::UnixGuard if self.writable_directories.len() != 2 => {
                return Err(io::Error::other(
                    "Unix keeper requires exactly bpffs and recovery directories",
                ));
            }
            _ => {}
        }
        let mut args: Vec<OsString> = [
            "-n",
            "/usr/bin/systemd-run",
            "--quiet",
            "--wait",
            "--pipe",
            "--expand-environment=no",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        args.push(format!("--unit={}", self.unit).into());
        for property in [
            "Type=exec".to_owned(), "RemainAfterExit=yes".into(),
            format!("RuntimeMaxSec={}s", self.maximum_seconds), "TimeoutStopSec=1s".into(),
            "KillMode=control-group".into(), "KillSignal=SIGKILL".into(), "SendSIGKILL=yes".into(), "MemoryMax=268435456".into(), "MemorySwapMax=0".into(),
            "TasksMax=8".into(), "CPUQuota=100%".into(), "CPUQuotaPeriodSec=100ms".into(),
            "LimitNOFILE=128".into(), "LimitFSIZE=1048576".into(), "LimitCORE=0".into(),
            "ProtectSystem=strict".into(), "ProtectHome=read-only".into(), "RestrictNamespaces=yes".into(),
            "Environment=PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C LC_ALL=C".into(),
            "UnsetEnvironment=LD_PRELOAD LD_LIBRARY_PATH LD_AUDIT LD_DEBUG LD_DEBUG_OUTPUT LD_PROFILE LD_PROFILE_OUTPUT LD_BIND_NOT LD_DYNAMIC_WEAK LD_ORIGIN_PATH GLIBC_TUNABLES".into(),
            "CapabilityBoundingSet=CAP_BPF CAP_PERFMON CAP_NET_ADMIN CAP_SYS_RESOURCE CAP_SYS_PTRACE".into(),
            "NoNewPrivileges=yes".into(),
        ] { args.push(format!("--property={property}").into()); }
        let mut writable = Vec::new();
        for directory in self.writable_directories {
            // systemd string-list parsing interprets whitespace and escapes;
            // refusing these names avoids expanding one owned directory into
            // additional host paths. No escaping/shell substitution is guessed.
            let text = directory
                .to_str()
                .ok_or_else(|| io::Error::other("non-UTF8 writable directory"))?;
            if !directory.is_absolute()
                || !text.split('/').any(|component| !component.is_empty())
                || text
                    .split('/')
                    .any(|component| matches!(component, "." | ".."))
                || text
                    .bytes()
                    .any(|byte| byte.is_ascii_whitespace() || b"\\\"'%".contains(&byte))
            {
                return Err(io::Error::other("unsupported writable-directory spelling"));
            }
            writable.push(text);
        }
        if !writable.is_empty() {
            args.push(format!("--property=ReadWritePaths={}", writable.join(" ")).into());
        }
        args.push("--".into());
        args.push(self.executable.as_os_str().to_owned());
        args.extend_from_slice(self.arguments);
        Ok(args)
    }

    /// Prepare an unstarted command using duplicates of actual borrowed owners.
    /// Spawn failure cannot lose the caller's original stdin/stdio capability.
    /// The returned child would be the sudo/systemd wrapper, not the helper.
    pub fn command(
        &self,
        stdin: &OwnedFd,
        stdout: &OwnedFd,
        stderr: &OwnedFd,
    ) -> io::Result<Command> {
        fn duplicate(fd: &OwnedFd) -> io::Result<OwnedFd> {
            let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
        }
        let mut command = Command::new(CAPABILITY_SUDO);
        command
            .env_clear()
            .envs(CAPABILITY_ENVIRONMENT.iter().copied())
            .args(self.arguments()?)
            .stdin(Stdio::from(duplicate(stdin)?))
            .stdout(Stdio::from(duplicate(stdout)?))
            .stderr(Stdio::from(duplicate(stderr)?));
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capability_unit_keeps_original_limits_and_no_shell() {
        let args = [OsString::from("--accepted-private-stdin-v1")];
        let spec = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::Accepted,
            unit: "hermit-accepted-01000000000000000000000000000000.service",
            executable: Path::new("/product/hermit"),
            arguments: &args,
            maximum_seconds: 30,
            writable_directories: &[],
        };
        let argv = spec.arguments().unwrap();
        for expected in [
            "--pipe",
            "--expand-environment=no",
            "--property=RuntimeMaxSec=30s",
            "--property=MemoryMax=268435456",
            "--property=TasksMax=8",
            "--property=LimitNOFILE=128",
            "--property=LimitFSIZE=1048576",
            "--property=KillSignal=SIGKILL",
            "--property=SendSIGKILL=yes",
            "--property=NoNewPrivileges=yes",
        ] {
            assert!(argv.contains(&OsString::from(expected)), "{expected}");
        }
        assert_eq!(
            &argv[argv.len() - 3..],
            &[
                OsString::from("--"),
                OsString::from("/product/hermit"),
                args[0].clone()
            ]
        );
    }
    #[test]
    fn capability_unit_rejects_identity_and_scope_expansion() {
        let bad_names = [
            "hermit-accepted-00000000000000000000000000000000.service",
            "hermit-accepted-short.service",
            "hermit-unix-01000000000000000000000000000000.service",
        ];
        for unit in bad_names {
            let spec = CapabilityUnitLaunch {
                kind: CapabilityServiceKind::Accepted,
                unit,
                executable: Path::new("/product/hermit"),
                arguments: &[],
                maximum_seconds: 30,
                writable_directories: &[],
            };
            assert!(spec.arguments().is_err());
        }
        let writes = [
            PathBuf::from("/owned/bpf"),
            PathBuf::from("/owned/recovery"),
        ];
        let spec = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::Accepted,
            unit: "hermit-accepted-01000000000000000000000000000000.service",
            executable: Path::new("/product/hermit"),
            arguments: &[],
            maximum_seconds: 30,
            writable_directories: &writes,
        };
        assert!(spec.arguments().is_err());
        for path in [
            "/",
            "//",
            "/owned/../..",
            "/owned/.",
            "relative",
            "/one /two",
            "/one\\x20/two",
            "/%h",
        ] {
            let paths = [PathBuf::from(path), PathBuf::from("/owned/recovery")];
            let spec = CapabilityUnitLaunch {
                kind: CapabilityServiceKind::UnixGuard,
                unit: "hermit-unix-01000000000000000000000000000000.service",
                executable: Path::new("/product/keeper"),
                arguments: &[],
                maximum_seconds: 30,
                writable_directories: &paths,
            };
            assert!(spec.arguments().is_err(), "{path}");
        }
    }
}
