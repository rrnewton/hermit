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
use std::os::fd::FromRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use reverie_elf_loader::Error;
use reverie_elf_loader::Invocation;
use reverie_elf_loader::prepare_start_with_limits;

use super::Artifacts;
use super::Case;
use super::Fixture;
use super::Mutation;
use super::Naming;
use super::TestResult;
use super::compare;
use super::error;
use super::geometry;
use super::io as check_io;
use super::run_pair;

const HUGE_PAGE: usize = 2 * 1024 * 1024;
const DIAGNOSTIC: &[u8] =
    b"reverie-elf-loader: hugetlb ELF file mapping semantics unsupported -22\n";

pub fn hugetlb_inode_controls() -> TestResult {
    for fixture in [Fixture::EntryPie, Fixture::EntryNonPie] {
        let mut case = Case::new("hugetlb-inode", fixture, Naming::Absolute, None)?;
        let ordinary = check_io(fs::read(fixture.path()))?;
        geometry::install_main(&mut case, &ordinary)?;
        let pair = run_pair(&case, Mutation::None)?;
        compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
        compare::observers(&pair).map_err(|failure| failure.to_string())?;

        let huge = huge_memfd()?;
        witness_syscall_rounding(&case, &huge)?;
        let huge_path = case.root.join("huge-inode");
        match fs::remove_file(&huge_path) {
            Ok(()) => {}
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => {}
            Err(cause) => return Err(cause.to_string()),
        }
        check_io(std::os::unix::fs::symlink(
            format!("/proc/self/fd/{}", huge.as_raw_fd()),
            &huge_path,
        ))?;
        let original_target = std::mem::replace(&mut case.target, check_io(huge.try_clone())?);
        let original_invocation = std::mem::replace(
            &mut case.invocation,
            error(Invocation::execve(huge_path.as_os_str()))?,
        );
        refuse(&case, &pair.artifacts, false)?;
        native_exec_error(&case, libc::ENOEXEC, "main")?;
        case.target = original_target;
        case.invocation = original_invocation;

        // Only this owned huge descriptor survives the loader exec, allowing
        // its interpreter pathname to pin exactly the same huge inode.
        // SAFETY: fcntl changes only this test-owned descriptor's exec flag.
        if unsafe { libc::fcntl(huge.as_raw_fd(), libc::F_SETFD, 0) } < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        let mut main = ordinary.clone();
        geometry::set_interpreter(&mut main, &huge_path);
        geometry::install_main(&mut case, &main)?;
        refuse(&case, &pair.artifacts, true)?;
        native_exec_error(&case, libc::ELIBBAD, "interpreter")?;
        // SAFETY: restore the flag on exactly the same owned descriptor.
        if unsafe { libc::fcntl(huge.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        geometry::install_main(&mut case, &ordinary)?;
        let restored = run_pair(&case, Mutation::None)?;
        compare::entries(&case, &restored).map_err(|failure| failure.to_string())?;
        compare::observers(&restored).map_err(|failure| failure.to_string())?;
        check_io(fs::write(
            case.root.join("hugetlb-control.witness"),
            format!(
                "PASS ordinary_before_after_full_entry_observer_parity=true huge_inode_magic={:#x} huge_inode={} main_host=HugetlbElf(false) interpreter_host=HugetlbElf(true) runtime_exit127_exact_stderr=true native_empty_main_errno={} native_empty_interpreter_errno={} native_valid_huge_elf_executed=false no_huge_pool_or_system_setting_changed=true\n",
                libc::HUGETLBFS_MAGIC,
                check_io(huge.metadata())?.ino(),
                libc::ENOEXEC,
                libc::ELIBBAD,
            ),
        ))?;
        println!(
            "hugetlb {}: actual huge inode and 4KiB syscall/2MiB VMA witness; exact main/interpreter HugetlbElf and defensive refusal; empty native mainENOEXEC/interpreterELIBBAD are rejection controls; ordinary restored full parity",
            fixture.label()
        );
    }
    Ok(())
}

fn huge_memfd() -> TestResult<File> {
    // SAFETY: the static name is NUL terminated. No huge page pool is changed;
    // creating/sizing this owned inode does not allocate or fault a huge page.
    let fd = unsafe {
        libc::memfd_create(
            c"reverie-elf-loader-hugetlb-control".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_HUGETLB | libc::MFD_HUGE_2MB,
        )
    };
    if fd < 0 {
        return Err(format!(
            "mandatory hugetlb inode control could not create MFD_HUGETLB: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: memfd_create returned a new owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    check_io(file.set_len(HUGE_PAGE as u64))?;
    check_io(file.set_permissions(Permissions::from_mode(0o755)))?;
    let metadata = check_io(file.metadata())?;
    if !metadata.is_file()
        || metadata.len() != HUGE_PAGE as u64
        || metadata.mode() & 0o7777 != 0o755
    {
        return Err("owned hugetlb control has unexpected inode/type/length/mode".into());
    }
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: filesystem has sufficient writable storage for fstatfs.
    if unsafe { libc::fstatfs(fd, filesystem.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    // SAFETY: the successful syscall initialized filesystem.
    if unsafe { filesystem.assume_init() }.f_type != libc::HUGETLBFS_MAGIC {
        return Err("MFD_HUGETLB descriptor is not independently hugetlbfs".into());
    }
    Ok(file)
}

fn witness_syscall_rounding(case: &Case, huge: &File) -> TestResult {
    // SAFETY: PROT_NONE and NORESERVE make this an unfaulted mapping of only
    // the owned huge inode. No huge pages are allocated, read or written.
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_NONE,
            libc::MAP_SHARED | libc::MAP_NORESERVE,
            huge.as_raw_fd(),
            0,
        )
    };
    if mapping == libc::MAP_FAILED {
        return Err(format!(
            "mandatory unfaulted huge syscall-rounding witness could not map: {}",
            io::Error::last_os_error()
        ));
    }
    let maps = fs::read_to_string("/proc/self/maps");
    // SAFETY: the syscall huge-rounded the known 4KiB request to one 2MiB
    // hugetlb page; unmap that entire owned mapping without faulting it.
    if unsafe { libc::munmap(mapping, HUGE_PAGE) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    let maps = check_io(maps)?;
    let start = mapping as usize;
    let range = format!("{start:x}-{:x}", start + HUGE_PAGE);
    let inode = check_io(huge.metadata())?.ino().to_string();
    let row = maps
        .lines()
        .find(|row| {
            let fields: Vec<_> = row.split_whitespace().collect();
            fields.len() >= 6
                && fields[0] == range
                && fields[1] == "---s"
                && fields[2] == "00000000"
                && fields[4] == inode
                && row.contains("reverie-elf-loader-hugetlb-control")
        })
        .ok_or_else(|| format!("4KiB huge SYS_mmap request lacks exact 2MiB VMA:\n{maps}"))?;
    check_io(fs::write(
        case.root.join("hugetlb-syscall-rounding.witness"),
        format!(
            "EXECUTED SYS_mmap_request=4096 VMA_size={HUGE_PAGE} PROT_NONE=true MAP_SHARED_NORESERVE=true page_faults_requested=false mapping_unmapped=true\n{row}\n"
        ),
    ))
}

fn refuse(case: &Case, artifacts: &Artifacts, interpreter: bool) -> TestResult {
    let result = prepare_start_with_limits(
        &case.target,
        &case.invocation,
        Path::new("./h"),
        case.limits,
    );
    if !matches!(result, Err(Error::HugetlbElf { interpreter: observed }) if observed == interpreter)
    {
        return Err(format!(
            "hugetlb inode gave wrong named preparation refusal (interpreter={interpreter}): {result:?}"
        ));
    }
    let label = if interpreter { "interpreter" } else { "main" };
    let loaded =
        geometry::untraced_start(case, Some(artifacts), &format!("hugetlb-{label}-loader"))?;
    if loaded.status.code() != Some(127) || !loaded.stdout.is_empty() || loaded.stderr != DIAGNOSTIC
    {
        return Err(format!(
            "hugetlb {label} inode escaped its exact defensive refusal: {loaded:?}"
        ));
    }
    Ok(())
}

fn native_exec_error(case: &Case, expected: i32, label: &str) -> TestResult {
    match Command::new(
        case.invocation
            .path()
            .to_str()
            .map_err(|cause| cause.to_string())?,
    )
    .current_dir(&case.root)
    .env_clear()
    .output()
    {
        Err(cause) if cause.raw_os_error() == Some(expected) => check_io(fs::write(
            case.root
                .join(format!("hugetlb-{label}-native-rejection.witness")),
            format!(
                "EXECUTED native_exec_errno={expected} empty_huge_inode=true not_parity=true\n"
            ),
        )),
        result => Err(format!(
            "empty huge {label} native exec must reject with errno{expected}: {result:?}"
        )),
    }
}
