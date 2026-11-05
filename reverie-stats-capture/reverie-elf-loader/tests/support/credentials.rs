/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::fs::File;
use std::fs::Permissions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::process::Output;

use reverie_elf_loader::Error;
use reverie_elf_loader::Invocation;
use reverie_elf_loader::prepare_start_with_limits;

use super::Artifacts;
use super::Case;
use super::Fixture;
use super::Mutation;
use super::Naming;
use super::Pair;
use super::TestResult;
use super::compare;
use super::error;
use super::geometry;
use super::io as check_io;
use super::output_root;
use super::run_pair;
use super::trace;

const MODE_REASON: &str = "setid mode";
const CAPABILITY_REASON: &str = "security.capability";
const CAPABILITY_NAME: &[u8] = b"security.capability\0";
const REFUSAL_DIAGNOSTIC: &[u8] =
    b"reverie-elf-loader: privileged executable would change exec credentials -22\n";

pub fn privileged_executable_mode_controls() -> TestResult {
    for fixture in [Fixture::EntryPie, Fixture::EntryNonPie] {
        for (label, mode) in [("setuid", 0o4755), ("setgid", 0o2755)] {
            let case = owned_case(&format!("privileged-mode-{label}"), fixture)?;
            let pair = accepted_pair(&case)?;
            let path = case.root.join("target");
            check_io(fs::set_permissions(&path, Permissions::from_mode(mode)))?;
            let metadata = check_io(case.target.metadata())?;
            if metadata.mode() & 0o7777 != mode {
                return Err(format!(
                    "owned credential control did not retain exact {mode:#o} mode: {:#o}",
                    metadata.mode() & 0o7777
                ));
            }
            refused_executable(&case, &pair.artifacts, MODE_REASON, label)?;
            check_io(fs::set_permissions(&path, Permissions::from_mode(0o755)))?;
            accepted_pair(&case)?;
            println!(
                "credential {} {label}: own-ID {mode:#o} native observer succeeds; exact PrivilegedExecutable(mode), defensive exit127; restored 0755 exact entry/observer parity",
                fixture.label()
            );
        }
    }
    Ok(())
}

pub fn privileged_executable_capability_controls() -> TestResult {
    for fixture in [Fixture::EntryPie, Fixture::EntryNonPie] {
        let case = owned_case("privileged-file-capability", fixture)?;
        let pair = accepted_pair(&case)?;
        // A valid revision-2 capability xattr grants a permitted CHOWN bit.
        // Its effective flag is deliberately unset, so this control does not
        // depend on permission to gain an effective capability under ptrace.
        let mut capabilities = [0_u8; 20];
        capabilities[..4].copy_from_slice(&0x0200_0000_u32.to_le_bytes());
        capabilities[4..8].copy_from_slice(&1_u32.to_le_bytes());
        let mut method = "direct fsetxattr: CHOWN permitted, effective flag clear";
        // SAFETY: descriptor pins this test-owned target; the name is NUL
        // terminated, and the value is live for its complete 20-byte length.
        let result = unsafe {
            libc::fsetxattr(
                case.target.as_raw_fd(),
                CAPABILITY_NAME.as_ptr().cast(),
                capabilities.as_ptr().cast(),
                capabilities.len(),
                libc::XATTR_CREATE,
            )
        };
        if result != 0 {
            let cause = io::Error::last_os_error();
            if !matches!(
                cause.raw_os_error(),
                Some(libc::EPERM | libc::EACCES | libc::EOPNOTSUPP)
            ) {
                return Err(format!("set owned file capability xattr: {cause}"));
            }
            let fallback = if matches!(cause.raw_os_error(), Some(libc::EPERM | libc::EACCES)) {
                harmless_capability_helper(&case, false)?
            } else {
                None
            };
            if let Some(output) = fallback
                && output.status.success()
            {
                // The approved helper installs an empty capability set. It
                // grants no capabilities and still exercises the production
                // guard's refusal of any security.capability xattr presence.
                capabilities[4..].fill(0);
                method = "sudo setcap empty: all capability words zero";
            } else {
                let failure = format!(
                    "file-capability native/refusal controls could not execute: direct fsetxattr(security.capability) on test-owned target: {cause}; empty-capability helper missing, denied or unsupported\n"
                );
                check_io(fs::write(
                    case.root.join("capability-control.witness"),
                    &failure,
                ))?;
                return Err(failure);
            }
        }
        let mut observed = [0_u8; 64];
        // SAFETY: the descriptor/name are live, and observed is writable for
        // the full advertised capacity. The returned count is checked below.
        let count = unsafe {
            libc::fgetxattr(
                case.target.as_raw_fd(),
                CAPABILITY_NAME.as_ptr().cast(),
                observed.as_mut_ptr().cast(),
                observed.len(),
            )
        };
        if count < 0 {
            return Err(format!(
                "read back owned file capability xattr: {}",
                io::Error::last_os_error()
            ));
        }
        // Filesystems may represent revision 2 as a revision-3 namespaced
        // capability. Require the exact selected permitted/inheritable words
        // and zero effective flag in either valid representation. A helper
        // success without the verified zero-capability xattr is a failure.
        let count = count as usize;
        let revision = u32::from_le_bytes(observed[..4].try_into().expect("four bytes"));
        if (count != 20 && count != 24)
            || revision & 0x00ff_ffff != 0
            || !matches!(revision >> 24, 2 | 3)
            || observed[4..20] != capabilities[4..20]
        {
            return Err(format!(
                "owned capability xattr readback differs: length={count}, bytes={:?}",
                &observed[..count]
            ));
        }
        refused_executable(&case, &pair.artifacts, CAPABILITY_REASON, "capability")?;
        // SAFETY: removes only the xattr placed on this test-owned file above.
        if unsafe { libc::fremovexattr(case.target.as_raw_fd(), CAPABILITY_NAME.as_ptr().cast()) }
            != 0
        {
            let cause = io::Error::last_os_error();
            if !matches!(cause.raw_os_error(), Some(libc::EPERM | libc::EACCES)) {
                return Err(format!("remove owned file capability xattr: {cause}"));
            }
            let removal = harmless_capability_helper(&case, true)?;
            if !removal
                .as_ref()
                .is_some_and(|output| output.status.success())
            {
                return Err(format!(
                    "remove owned file capability xattr: {cause}; approved owned-file helper unavailable or denied"
                ));
            }
        }
        // SAFETY: a zero-size query supplies no writable data pointer.
        let absent = unsafe {
            libc::fgetxattr(
                case.target.as_raw_fd(),
                CAPABILITY_NAME.as_ptr().cast(),
                std::ptr::null_mut(),
                0,
            )
        };
        if absent != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::ENODATA) {
            return Err("owned capability xattr was not independently removed".into());
        }
        accepted_pair(&case)?;
        let witness_path = case.root.join("capability-control.witness");
        let mut witness = check_io(fs::read_to_string(&witness_path))?;
        witness.push_str(&format!(
            "EXECUTED method={method:?} exact_xattr_readback={:?} xattr_independently_removed=true restored_0755_full_entry_observer_parity=true\n",
            &observed[..count]
        ));
        check_io(fs::write(witness_path, witness))?;
        println!(
            "credential {} file capability ({method}): native observer succeeds; exact PrivilegedExecutable(capability), defensive exit127; xattr removed, exact ordinary entry/observer parity",
            fixture.label(),
        );
    }
    Ok(())
}

fn harmless_capability_helper(case: &Case, remove: bool) -> TestResult<Option<Output>> {
    let approved_root = output_root()?.join("privileged-file-capability");
    let relative = case
        .root
        .strip_prefix(&approved_root)
        .map_err(|_| "capability helper target is outside its approved test directory")?;
    if relative != Path::new("entry-pie-absolute") && relative != Path::new("entry-nonpie-absolute")
    {
        return Err(
            "capability helper target is not one of its two approved owned fixtures".into(),
        );
    }
    let path = case.root.join("target");
    let named = check_io(fs::symlink_metadata(&path))?;
    let pinned = check_io(case.target.metadata())?;
    // SAFETY: geteuid has no pointer arguments or side effects.
    let owner = unsafe { libc::geteuid() };
    if !named.is_file()
        || named.uid() != owner
        || named.dev() != pinned.dev()
        || named.ino() != pinned.ino()
    {
        return Err("capability helper pathname does not bind the pinned test-owned inode".into());
    }
    if !Path::new("/usr/bin/sudo").is_file() || !Path::new("/usr/sbin/setcap").is_file() {
        return Ok(None);
    }
    let operation = if remove { "remove" } else { "empty" };
    let output = match Command::new("/usr/bin/sudo")
        .arg("-n")
        .arg("/usr/sbin/setcap")
        .arg(if remove { "-r" } else { "" })
        .arg(&path)
        .output()
    {
        Ok(output) => output,
        Err(cause)
            if matches!(
                cause.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            check_io(fs::write(
                case.root
                    .join(format!("capability-helper-{operation}.status")),
                format!("not executed: {cause}\n"),
            ))?;
            return Ok(None);
        }
        Err(cause) => return Err(cause.to_string()),
    };
    check_io(fs::write(
        case.root
            .join(format!("capability-helper-{operation}.stdout")),
        &output.stdout,
    ))?;
    check_io(fs::write(
        case.root
            .join(format!("capability-helper-{operation}.stderr")),
        &output.stderr,
    ))?;
    check_io(fs::write(
        case.root
            .join(format!("capability-helper-{operation}.status")),
        format!("{}\n", output.status),
    ))?;
    Ok(Some(output))
}

fn owned_case(test: &str, fixture: Fixture) -> TestResult<Case> {
    let mut case = Case::new(test, fixture, Naming::Absolute, None)?;
    let path = case.root.join("target");
    // Repeated test invocations create a fresh inode, so mode/xattrs left by a
    // failed earlier control cannot contaminate the accepted companion.
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => {}
        Err(cause) => return Err(cause.to_string()),
    }
    let image = check_io(fs::read(fixture.path()))?;
    geometry::write_fixture(
        &path,
        &image,
        "fresh unchanged ELF copy for owned credential mode/xattr controls",
    )?;
    case.target = check_io(File::open(&path))?;
    case.invocation = error(Invocation::execve(path.as_os_str()))?;
    let metadata = check_io(case.target.metadata())?;
    // SAFETY: get[e]uid/get[e]gid have no pointer arguments or side effects.
    let (owner, group) = unsafe { (libc::geteuid(), libc::getegid()) };
    if metadata.uid() != owner || metadata.gid() != group {
        return Err(format!(
            "credential control is not owned by the creating process's effective IDs: uid={} gid={} expected uid={owner} gid={group}",
            metadata.uid(),
            metadata.gid()
        ));
    }
    Ok(case)
}

fn accepted_pair(case: &Case) -> TestResult<Pair> {
    let pair = run_pair(case, Mutation::None)?;
    compare::entries(case, &pair).map_err(|failure| failure.to_string())?;
    compare::observers(&pair).map_err(|failure| failure.to_string())?;
    Ok(pair)
}

fn refused_executable(
    case: &Case,
    artifacts: &Artifacts,
    reason: &'static str,
    label: &str,
) -> TestResult {
    match prepare_start_with_limits(
        &case.target,
        &case.invocation,
        Path::new("./h"),
        case.limits,
    ) {
        Err(Error::PrivilegedExecutable { reason: observed }) if observed == reason => {}
        result => {
            return Err(format!(
                "{label} target gave wrong named preparation refusal: {result:?}"
            ));
        }
    }
    let native = trace::run(case, None)?;
    if native.exit != 0 || !native.stderr.is_empty() {
        return Err(format!(
            "native {label} target did not execute its observer: status={} stderr={:?}",
            native.exit, native.stderr
        ));
    }
    compare::observer_witness(&native, "native credential control")
        .map_err(|failure| failure.to_string())?;
    let loaded = geometry::untraced_start(case, Some(artifacts), &format!("{label}-loader"))?;
    if loaded.status.code() != Some(127)
        || !loaded.stdout.is_empty()
        || loaded.stderr != REFUSAL_DIAGNOSTIC
    {
        return Err(format!(
            "{label} target escaped defensive credential refusal using its pre-change prepared image: {loaded:?}"
        ));
    }
    for (suffix, bytes) in [
        ("maps", &native.entry.maps),
        ("stdout", &native.stdout),
        ("stderr", &native.stderr),
    ] {
        check_io(fs::write(
            case.root.join(format!("{label}-native.{suffix}")),
            bytes,
        ))?;
    }
    check_io(fs::write(
        case.root.join(format!("{label}-control.witness")),
        format!(
            "PASS host=PrivilegedExecutable({reason:?}) native_status=0 native_independent_observer=true loader_status=127 exact_stderr=true owned_mode={:#o} native_auxv={:?} native_elapsed={:.6}s\n",
            check_io(case.target.metadata())?.mode() & 0o7777,
            native.entry.aux,
            native.elapsed.as_secs_f64()
        ),
    ))
}
