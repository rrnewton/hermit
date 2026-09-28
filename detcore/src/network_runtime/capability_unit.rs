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
/// Descriptor limit for every capability unit. The grouped accepted provider
/// holds its 23 maps, 47 programs and 47 links (117 original IDs) at once, plus
/// perf events, tracefs leaves, pidfds and journals, and libbpf opens transient
/// descriptors for feature probes during attach. At 128 the probe for kernel
/// perf links failed with EMFILE, so libbpf reported cookie attach as
/// unsupported and startup refused with EOPNOTSUPP.
pub const CAPABILITY_UNIT_NOFILE: u64 = 256;
/// The accepted loader verifies and attaches all 47 programs before it can
/// drop libbpf's transient state. FtraceV1 hit the former 256 MiB cgroup cap
/// exactly (MemoryPeak=268435456, Result=oom-kill) before publishing READY.
/// Keep one additional former-cap of bounded verifier/load headroom; the
/// metadata-only readers and Unix keeper retain their smaller existing cap.
pub const ACCEPTED_UNIT_MEMORY_MAX: u64 = 512 * 1024 * 1024;
/// Existing cap retained for keepers and metadata-only readback helpers.
pub const OTHER_CAPABILITY_UNIT_MEMORY_MAX: u64 = 256 * 1024 * 1024;

/// Separate lifecycle owners share limits and loader hygiene, not authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityServiceKind {
    /// Readonly accepted TCP observer, started after clone before guest start.
    Accepted,
    /// Metadata/custody keeper for the retained GroupedV1 compatibility route.
    /// Unlike the BPF-loading Accepted creator, this process is admitted by the
    /// original 256-MiB keeper policy and never receives verifier headroom.
    AcceptedKeeper,
    /// Unix policy keeper, started and armed before the container clone.
    UnixGuard,
    /// Short-lived metadata-only executable querying original BPF object IDs.
    UnixReadback,
    /// The same metadata-only query policy for accepted-provider original IDs.
    AcceptedReadback,
}

/// Service lifetime policy, separate from finite startup and cleanup deadlines.
/// This value configures a launcher; it never authenticates a controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityServiceLifetime {
    /// A bounded qualification or explicitly bounded enclosing run. Zero is
    /// rejected, and the exact existing bound is passed to systemd unchanged.
    Bounded(u32),
    /// The service monitors its authenticated held parent/controller pidfds.
    /// Callers must also provide a finite pre-authentication bootstrap deadline
    /// and explicit terminal cleanup. Channel closure alone is not that proof.
    /// This variant does not impose a new wall deadline on an untimed guest.
    ControllerOwned,
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
    /// Explicit lifetime policy; startup and shutdown remain separately bounded.
    pub lifetime: CapabilityServiceLifetime,
    /// Unix keeper only: exact owned bpffs and recovery directories. These are
    /// mount-policy exceptions, not evidence that a pathname still owns an FD.
    pub writable_directories: &'a [PathBuf],
}

impl CapabilityUnitLaunch<'_> {
    /// Validate and materialize argv before a fork-safe raw exec stub. This is
    /// the same policy used by `command`; it does not launch or transfer owners.
    pub fn arguments(&self) -> io::Result<Vec<OsString>> {
        let prefix = match self.kind {
            CapabilityServiceKind::Accepted | CapabilityServiceKind::AcceptedKeeper => {
                "hermit-accepted-"
            }
            CapabilityServiceKind::UnixGuard => "hermit-unix-",
            CapabilityServiceKind::UnixReadback => "hermit-unix-readback-",
            CapabilityServiceKind::AcceptedReadback => "hermit-accepted-readback-",
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
            || self.lifetime == CapabilityServiceLifetime::Bounded(0)
        {
            return Err(io::Error::other(
                "invalid capability unit identity, executable, or bound",
            ));
        }
        match self.kind {
            CapabilityServiceKind::Accepted
            | CapabilityServiceKind::AcceptedKeeper
            | CapabilityServiceKind::UnixReadback
            | CapabilityServiceKind::AcceptedReadback
                if !self.writable_directories.is_empty() =>
            {
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
        if matches!(self.kind, CapabilityServiceKind::UnixReadback)
            && !matches!(self.lifetime, CapabilityServiceLifetime::Bounded(_))
        {
            return Err(io::Error::other(
                "metadata readback requires a finite unit lifetime",
            ));
        }
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        if uid != unsafe { libc::geteuid() } || gid != unsafe { libc::getegid() } {
            return Err(io::Error::other(
                "capability launcher requires actual matching owner identity",
            ));
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
        let memory_max = match self.kind {
            CapabilityServiceKind::Accepted => ACCEPTED_UNIT_MEMORY_MAX,
            _ => OTHER_CAPABILITY_UNIT_MEMORY_MAX,
        };
        let remain_after_exit = match self.kind {
            CapabilityServiceKind::Accepted
            | CapabilityServiceKind::AcceptedKeeper
            | CapabilityServiceKind::AcceptedReadback => "no",
            CapabilityServiceKind::UnixGuard | CapabilityServiceKind::UnixReadback => "yes",
        };
        for property in [
            "Type=exec".to_owned(), format!("RemainAfterExit={remain_after_exit}"),
            "TimeoutStopSec=1s".into(),
            "KillMode=control-group".into(), "KillSignal=SIGKILL".into(), "SendSIGKILL=yes".into(), format!("MemoryMax={memory_max}"), "MemorySwapMax=0".into(),
            "TasksMax=8".into(), "CPUQuota=100%".into(), "CPUQuotaPeriodSec=100ms".into(),
            format!("LimitNOFILE={CAPABILITY_UNIT_NOFILE}"), "LimitFSIZE=1048576".into(), "LimitCORE=0".into(),
            "ProtectSystem=strict".into(), "ProtectHome=read-only".into(), "RestrictNamespaces=yes".into(),
            "Environment=PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C LC_ALL=C".into(),
            "UnsetEnvironment=LD_PRELOAD LD_LIBRARY_PATH LD_AUDIT LD_DEBUG LD_DEBUG_OUTPUT LD_PROFILE LD_PROFILE_OUTPUT LD_BIND_NOT LD_DYNAMIC_WEAK LD_ORIGIN_PATH GLIBC_TUNABLES".into(),
            "NoNewPrivileges=yes".into(),
        ] { args.push(format!("--property={property}").into()); }
        // Run as the owner of the held private directories. No DAC override is
        // added: the exact same loader capabilities are carried ambiently.
        let capabilities = match self.kind {
            CapabilityServiceKind::UnixReadback | CapabilityServiceKind::AcceptedReadback => {
                "CAP_SYS_ADMIN"
            }
            CapabilityServiceKind::Accepted
            | CapabilityServiceKind::AcceptedKeeper
            | CapabilityServiceKind::UnixGuard => {
                "CAP_BPF CAP_PERFMON CAP_NET_ADMIN CAP_SYS_RESOURCE CAP_SYS_PTRACE"
            }
        };
        for property in [
            format!("User={uid}"),
            format!("Group={gid}"),
            format!("CapabilityBoundingSet={capabilities}"),
            format!("AmbientCapabilities={capabilities}"),
        ] {
            args.push(format!("--property={property}").into());
        }
        if let CapabilityServiceLifetime::Bounded(seconds) = self.lifetime {
            args.push(format!("--property=RuntimeMaxSec={seconds}s").into());
        }
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
    fn unix_readback_is_bounded_readonly_and_has_only_query_privilege() {
        let mut spec = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::UnixReadback,
            unit: "hermit-unix-readback-01000000000000000000000000000000.service",
            executable: Path::new("/product/hermit-unix-readback"),
            arguments: &[],
            lifetime: CapabilityServiceLifetime::Bounded(30),
            writable_directories: &[],
        };
        let args = spec.arguments().unwrap();
        for expected in [
            "--property=CapabilityBoundingSet=CAP_SYS_ADMIN".to_owned(),
            "--property=AmbientCapabilities=CAP_SYS_ADMIN".to_owned(),
            format!("--property=User={}", unsafe { libc::getuid() }),
            format!("--property=Group={}", unsafe { libc::getgid() }),
            "--property=RuntimeMaxSec=30s".to_owned(),
            "--property=MemoryMax=268435456".to_owned(),
        ] {
            assert!(args.contains(&OsString::from(expected)));
        }
        assert!(
            !args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("CAP_BPF"))
        );
        spec.lifetime = CapabilityServiceLifetime::ControllerOwned;
        assert!(spec.arguments().is_err());
        spec.lifetime = CapabilityServiceLifetime::Bounded(30);
        let paths = [PathBuf::from("/owned/query-write")];
        spec.writable_directories = &paths;
        assert!(spec.arguments().is_err());
    }
    #[test]
    fn capability_unit_keeps_original_limits_and_no_shell() {
        let args = [OsString::from("--accepted-private-stdin-v1")];
        let spec = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::Accepted,
            unit: "hermit-accepted-01000000000000000000000000000000.service",
            executable: Path::new("/product/hermit"),
            arguments: &args,
            lifetime: CapabilityServiceLifetime::Bounded(30),
            writable_directories: &[],
        };
        let argv = spec.arguments().unwrap();
        for expected in [
            "--pipe",
            "--expand-environment=no",
            "--property=RuntimeMaxSec=30s",
            "--property=MemoryMax=536870912",
            "--property=RemainAfterExit=no",
            "--property=TasksMax=8",
            "--property=LimitNOFILE=256",
            "--property=LimitFSIZE=1048576",
            "--property=KillSignal=SIGKILL",
            "--property=SendSIGKILL=yes",
            "--property=NoNewPrivileges=yes",
        ] {
            assert!(argv.contains(&OsString::from(expected)), "{expected}");
        }
        assert!(!argv.iter().any(|argument| {
            let argument = argument.to_string_lossy();
            argument.starts_with("--property=OpenFile=")
                || argument.contains("kprobe_events")
                || argument.contains("hermit_kprobe_control")
        }));
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
    fn grouped_keeper_uses_the_admitted_256_mib_policy_not_loader_headroom() {
        let spec = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::AcceptedKeeper,
            unit: "hermit-accepted-01000000000000000000000000000000.service",
            executable: Path::new("/product/hermit"),
            arguments: &[OsString::from("--grouped-runtime-keeper-private-stdin-v1")],
            lifetime: CapabilityServiceLifetime::ControllerOwned,
            writable_directories: &[],
        };
        let argv = spec.arguments().unwrap();
        assert!(argv.contains(&OsString::from("--property=MemoryMax=268435456")));
        assert!(!argv.contains(&OsString::from("--property=MemoryMax=536870912")));
        assert!(!argv.iter().any(|argument| {
            argument
                .to_string_lossy()
                .starts_with("--property=RuntimeMaxSec=")
        }));
    }
    #[test]
    fn accepted_readback_exits_with_its_helper_but_unix_lifetimes_stay_retained() {
        for (kind, prefix, remain) in [
            (
                CapabilityServiceKind::AcceptedReadback,
                "hermit-accepted-readback-",
                "no",
            ),
            (
                CapabilityServiceKind::UnixReadback,
                "hermit-unix-readback-",
                "yes",
            ),
            (CapabilityServiceKind::UnixGuard, "hermit-unix-", "yes"),
        ] {
            let unit = format!("{prefix}01000000000000000000000000000000.service");
            let paths = [
                PathBuf::from("/owned/bpf"),
                PathBuf::from("/owned/recovery"),
            ];
            let spec = CapabilityUnitLaunch {
                kind,
                unit: &unit,
                executable: Path::new("/product/helper"),
                arguments: &[],
                lifetime: CapabilityServiceLifetime::Bounded(30),
                writable_directories: if kind == CapabilityServiceKind::UnixGuard {
                    &paths
                } else {
                    &[]
                },
            };
            let args = spec.arguments().unwrap();
            assert!(args.contains(&OsString::from(format!(
                "--property=RemainAfterExit={remain}"
            ))));
        }
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
                lifetime: CapabilityServiceLifetime::Bounded(30),
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
            lifetime: CapabilityServiceLifetime::Bounded(30),
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
                lifetime: CapabilityServiceLifetime::Bounded(30),
                writable_directories: &paths,
            };
            assert!(spec.arguments().is_err(), "{path}");
        }
    }

    #[test]
    fn controller_owned_lifetime_changes_only_the_wall_property() {
        let paths = [
            PathBuf::from("/owned/bpf"),
            PathBuf::from("/owned/recovery"),
        ];
        let mut spec = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::UnixGuard,
            unit: "hermit-unix-01000000000000000000000000000000.service",
            executable: Path::new("/product/keeper"),
            arguments: &[],
            lifetime: CapabilityServiceLifetime::Bounded(30),
            writable_directories: &paths,
        };
        let bounded = spec.arguments().unwrap();
        spec.lifetime = CapabilityServiceLifetime::ControllerOwned;
        let owned = spec.arguments().unwrap();
        let expected: Vec<_> = bounded
            .into_iter()
            .filter(|arg| arg != "--property=RuntimeMaxSec=30s")
            .collect();
        assert_eq!(owned, expected);
        assert!(
            !owned
                .iter()
                .any(|arg| arg.to_string_lossy().contains("RuntimeMaxSec="))
        );
        spec.lifetime = CapabilityServiceLifetime::Bounded(0);
        assert!(spec.arguments().is_err());
    }
}

/// Fixed manager descriptions for grouped startup. This is launch configuration,
/// not evidence of creator, cgroup, descriptor or adoption ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupedOpenFiles<'a> {
    /// S1 receives only its exact three source controls.
    SourceControls,
    /// S2 receives only the three exact nonce-bound event leaves. Original
    /// controls arrive separately through the retained private SCM transfer.
    SuccessorLeaves {
        /// Exact nonzero lowercase hexadecimal run identity.
        nonce: &'a str,
    },
}
impl CapabilityUnitLaunch<'_> {
    /// Launch only the fixed trusted prepared-namespace setup entry. The caller
    /// must own an authenticated namespace through actual SourceTerminal and
    /// retain all three descriptors until the real leaf service is joined.
    /// These numbers describe retained objects; they do not create authority.
    /// Setup reinstalls the existing effective policy before original Creator
    /// admission. No ordinary Accepted service uses this preparation policy.
    pub(crate) fn arguments_with_prepared_namespace(
        &self,
        nonce: &str,
        namespace: i32,
        setup: i32,
        image: i32,
        mount_identity: (u64, u64),
        user_identity: (u64, u64),
        root_identity: (u64, u64),
    ) -> io::Result<Vec<OsString>> {
        if self.kind != CapabilityServiceKind::Accepted
            || !self.writable_directories.is_empty()
            || !matches!(self.lifetime, CapabilityServiceLifetime::Bounded(1..=20))
            || self.arguments.len() != 9
            || self.arguments[0] != "--grouped-leaves-private-stdin-v1"
            || self.arguments[1] != "--unit"
            || self.arguments[2] != self.unit
            || self.arguments[3] != "--run"
            || self.arguments[4] != nonce
            || self.arguments[5] != "--incarnation"
            || self.arguments[7] != "--deadline-ns"
            || [namespace, setup, image]
                .iter()
                .any(|fd| *fd < 3 || *fd >= 128)
            || namespace == setup
            || namespace == image
            || setup == image
            || [mount_identity.1, user_identity.1, root_identity.1].contains(&0)
        {
            return Err(io::Error::other(
                "prepared leaf launch lacks exact original roles and held objects",
            ));
        }
        let pid = unsafe { libc::getpid() };
        // The manager opens the held setup image before dropping privileges.
        // systemd-run resolves its command before manager OpenFile descriptors
        // exist, so the fixed trusted host env image execs the service's own fd8.
        // No shell, assignments or PATH search is involved. Manager start-job
        // success proves only env exec; original helper admission stays mandatory.
        let setup_path = PathBuf::from("/usr/bin/env");
        let mut arguments: Vec<OsString> = [
            "--".to_owned(),
            "/proc/self/fd/8".to_owned(),
            "--namespace".to_owned(),
            mount_identity.0.to_string(),
            mount_identity.1.to_string(),
            "--user-namespace".to_owned(),
            user_identity.0.to_string(),
            user_identity.1.to_string(),
            "--root".to_owned(),
            root_identity.0.to_string(),
            root_identity.1.to_string(),
            "--".to_owned(),
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        arguments.push(self.executable.as_os_str().to_owned());
        arguments.extend_from_slice(self.arguments);
        let setup_launch = CapabilityUnitLaunch {
            kind: self.kind,
            unit: self.unit,
            executable: &setup_path,
            arguments: &arguments,
            lifetime: self.lifetime,
            writable_directories: &[],
        };
        let mut args = setup_launch
            .arguments_with_grouped_files(GroupedOpenFiles::SuccessorLeaves { nonce })?;
        let original_caps = "CAP_BPF CAP_PERFMON CAP_NET_ADMIN CAP_SYS_RESOURCE CAP_SYS_PTRACE";
        let setup_caps = format!("{original_caps} CAP_SYS_ADMIN CAP_SYS_CHROOT CAP_SETPCAP");
        for (before, after) in [
            (
                "--property=ProtectSystem=strict".to_owned(),
                "--property=ProtectSystem=no".to_owned(),
            ),
            (
                "--property=ProtectHome=read-only".to_owned(),
                "--property=ProtectHome=no".to_owned(),
            ),
            (
                "--property=RestrictNamespaces=yes".to_owned(),
                "--property=RestrictNamespaces=mnt".to_owned(),
            ),
            (
                format!("--property=CapabilityBoundingSet={original_caps}"),
                format!("--property=CapabilityBoundingSet={setup_caps}"),
            ),
            (
                format!("--property=AmbientCapabilities={original_caps}"),
                format!("--property=AmbientCapabilities={setup_caps}"),
            ),
        ] {
            let positions: Vec<_> = args
                .iter()
                .enumerate()
                .filter(|(_, arg)| **arg == OsString::from(&before))
                .map(|(index, _)| index)
                .collect();
            if positions.len() != 1 {
                return Err(io::Error::other(
                    "original setup policy changed unexpectedly",
                ));
            }
            args[positions[0]] = after.into();
        }
        let separator = args
            .iter()
            .position(|arg| arg == "--")
            .ok_or_else(|| io::Error::other("prepared setup command separator absent"))?;
        args.splice(
            separator..separator,
            [
                format!(
                    "--property=OpenFile=/proc/{pid}/fd/{namespace}:hermit_prepared_mount:read-only"
                )
                .into(),
                format!(
                    "--property=OpenFile=/proc/{pid}/fd/{image}:hermit_prepared_image:read-only"
                )
                .into(),
                format!(
                    "--property=OpenFile=/proc/{pid}/fd/{setup}:hermit_prepared_setup:read-only"
                )
                .into(),
            ],
        );
        Ok(args)
    }

    /// Add only the fixed OpenFile roles to the original unchanged policy.
    /// Original absolute stage ownership is checked by the broker; rendering a
    /// service lifetime cannot restart that clock or issue a capability.
    pub fn arguments_with_grouped_files(
        &self,
        roles: GroupedOpenFiles<'_>,
    ) -> io::Result<Vec<OsString>> {
        if self.kind != CapabilityServiceKind::Accepted || !self.writable_directories.is_empty() {
            return Err(io::Error::other(
                "grouped roles require the accepted service policy",
            ));
        }
        let mut args = self.arguments()?;
        let files = match roles {
            GroupedOpenFiles::SourceControls => {
                if !matches!(self.lifetime, CapabilityServiceLifetime::Bounded(1..=20)) {
                    return Err(io::Error::other(
                        "source creator requires the original finite20s ceiling",
                    ));
                }
                vec![
                    "/sys/kernel/tracing/kprobe_events:hermit_kprobe_control".to_owned(),
                    "/sys/kernel/tracing/kprobe_profile:hermit_kprobe_profile:read-only".to_owned(),
                    "/sys/kernel/tracing/events:hermit_trace_events:read-only".to_owned(),
                ]
            }
            GroupedOpenFiles::SuccessorLeaves { nonce } => {
                if nonce.len() != 32
                    || nonce.bytes().all(|b| b == b'0')
                    || !nonce
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(io::Error::other(
                        "grouped leaf nonce must be exact nonzero lowercase hexadecimal",
                    ));
                }
                ["id", "format", "enable"].iter().map(|name|format!(
                    "/sys/kernel/tracing/events/hermit_{nonce}/hermit_classic_{nonce}/{name}:hermit_group_{name}:read-only"
                )).collect()
            }
        };
        let index = args
            .iter()
            .position(|arg| arg == "--")
            .ok_or_else(|| io::Error::other("capability command separator absent"))?;
        args.splice(
            index..index,
            files
                .into_iter()
                .map(|path| format!("--property=OpenFile={path}").into()),
        );
        Ok(args)
    }
}

#[cfg(test)]
mod grouped_role_tests {
    use super::*;
    #[test]
    fn prepared_leaf_keeps_bounds_and_requires_distinct_held_roles() {
        let nonce = "01000000000000000000000000000000";
        let unit = "hermit-accepted-02000000000000000000000000000000.service";
        let arguments: Vec<OsString> = [
            "--grouped-leaves-private-stdin-v1",
            "--unit",
            unit,
            "--run",
            nonce,
            "--incarnation",
            "1",
            "--deadline-ns",
            "1234",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        let spec = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::Accepted,
            unit,
            executable: Path::new("/trusted/hermit"),
            arguments: &arguments,
            lifetime: CapabilityServiceLifetime::Bounded(20),
            writable_directories: &[],
        };
        let base = spec
            .arguments_with_grouped_files(GroupedOpenFiles::SuccessorLeaves { nonce })
            .unwrap();
        let prepared = spec
            .arguments_with_prepared_namespace(nonce, 10, 11, 12, (4, 5), (4, 6), (7, 8))
            .unwrap();
        for arg in base.iter().take_while(|arg| *arg != "--") {
            let text = arg.to_string_lossy();
            if [
                "--property=ProtectSystem=",
                "--property=ProtectHome=",
                "--property=RestrictNamespaces=",
                "--property=CapabilityBoundingSet=",
                "--property=AmbientCapabilities=",
            ]
            .iter()
            .any(|prefix| text.starts_with(prefix))
            {
                continue;
            }
            assert!(
                prepared.contains(arg),
                "original bound/role disappeared: {text}"
            );
        }
        let roles: Vec<_> = prepared
            .iter()
            .filter(|s| s.to_string_lossy().starts_with("--property=OpenFile="))
            .cloned()
            .collect();
        let original_roles: Vec<_> = base
            .iter()
            .filter(|s| s.to_string_lossy().starts_with("--property=OpenFile="))
            .cloned()
            .collect();
        assert_eq!(roles.len(), 6);
        assert_eq!(&roles[..3], original_roles.as_slice());
        let pid = unsafe { libc::getpid() };
        assert_eq!(
            &roles[3..],
            &[
                OsString::from(format!(
                    "--property=OpenFile=/proc/{pid}/fd/10:hermit_prepared_mount:read-only"
                )),
                OsString::from(format!(
                    "--property=OpenFile=/proc/{pid}/fd/12:hermit_prepared_image:read-only"
                )),
                OsString::from(format!(
                    "--property=OpenFile=/proc/{pid}/fd/11:hermit_prepared_setup:read-only"
                )),
            ]
        );
        let separator = prepared.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(
            &prepared[separator + 1..separator + 4],
            &[
                OsString::from("/usr/bin/env"),
                OsString::from("--"),
                OsString::from("/proc/self/fd/8"),
            ]
        );
        assert!(prepared.contains(&OsString::from("--property=NoNewPrivileges=yes")));
        assert!(prepared.contains(&OsString::from("--property=RestrictNamespaces=mnt")));
        for (ns, setup, image) in [
            (10, 10, 12),
            (10, 11, 10),
            (10, 11, 11),
            (2, 11, 12),
            (10, 128, 12),
        ] {
            assert!(
                spec.arguments_with_prepared_namespace(
                    nonce,
                    ns,
                    setup,
                    image,
                    (4, 5),
                    (4, 6),
                    (7, 8)
                )
                .is_err()
            );
        }
        assert!(
            spec.arguments_with_prepared_namespace(nonce, 10, 11, 12, (4, 0), (4, 6), (7, 8))
                .is_err()
        );
        let other_role = CapabilityUnitLaunch {
            arguments: &[],
            ..spec
        };
        assert!(
            other_role
                .arguments_with_prepared_namespace(nonce, 10, 11, 12, (4, 5), (4, 6), (7, 8))
                .is_err()
        );
    }
    #[test]
    fn grouped_source_adds_only_exact_roles_without_changing_existing_policy() {
        let spec = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::Accepted,
            unit: "hermit-accepted-01000000000000000000000000000000.service",
            executable: Path::new("/product/hermit"),
            arguments: &[],
            lifetime: CapabilityServiceLifetime::Bounded(20),
            writable_directories: &[],
        };
        let base = spec.arguments().unwrap();
        let args = spec
            .arguments_with_grouped_files(GroupedOpenFiles::SourceControls)
            .unwrap();
        let roles: Vec<_> = args
            .iter()
            .filter(|s| s.to_string_lossy().starts_with("--property=OpenFile="))
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            roles,
            [
                "--property=OpenFile=/sys/kernel/tracing/kprobe_events:hermit_kprobe_control",
                "--property=OpenFile=/sys/kernel/tracing/kprobe_profile:hermit_kprobe_profile:read-only",
                "--property=OpenFile=/sys/kernel/tracing/events:hermit_trace_events:read-only"
            ]
        );
        assert_eq!(
            args.into_iter()
                .filter(|s| !s.to_string_lossy().starts_with("--property=OpenFile="))
                .collect::<Vec<_>>(),
            base
        );
    }
    #[test]
    fn grouped_successor_roles_refuse_noncanonical_nonce_and_source_time_extension() {
        let mut spec = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::Accepted,
            unit: "hermit-accepted-01000000000000000000000000000000.service",
            executable: Path::new("/product/hermit"),
            arguments: &[],
            lifetime: CapabilityServiceLifetime::ControllerOwned,
            writable_directories: &[],
        };
        let nonce = "1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a";
        let args = spec
            .arguments_with_grouped_files(GroupedOpenFiles::SuccessorLeaves { nonce })
            .unwrap();
        for name in ["id", "format", "enable"] {
            assert!(args.contains(&format!("--property=OpenFile=/sys/kernel/tracing/events/hermit_{nonce}/hermit_classic_{nonce}/{name}:hermit_group_{name}:read-only").into()));
        }
        assert_eq!(args.len(), spec.arguments().unwrap().len() + 3);
        for nonce in [
            "",
            "00000000000000000000000000000000",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "../../../../../../../../../../xx",
            "1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1 ",
        ] {
            assert!(
                spec.arguments_with_grouped_files(GroupedOpenFiles::SuccessorLeaves { nonce })
                    .is_err()
            );
        }
        assert!(
            spec.arguments_with_grouped_files(GroupedOpenFiles::SourceControls)
                .is_err()
        );
        spec.lifetime = CapabilityServiceLifetime::Bounded(21);
        assert!(
            spec.arguments_with_grouped_files(GroupedOpenFiles::SourceControls)
                .is_err()
        );
        spec.lifetime = CapabilityServiceLifetime::Bounded(20);
        spec.kind = CapabilityServiceKind::UnixReadback;
        assert!(
            spec.arguments_with_grouped_files(GroupedOpenFiles::SourceControls)
                .is_err()
        );
    }
}
