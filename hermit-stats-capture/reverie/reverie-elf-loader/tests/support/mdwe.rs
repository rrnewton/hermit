/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::Permissions;
use std::io;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::process::Output;
use std::time::Instant;

use reverie_elf_loader::Error;
use reverie_elf_loader::Invocation;
use reverie_elf_loader::Limits;
use reverie_elf_loader::prepare_start_with_limits;

use super::Artifacts;
use super::Case;
use super::Fixture;
use super::METADATA_FD;
use super::Mutation;
use super::Naming;
use super::TARGET_FD;
use super::TestResult;
use super::compare;
use super::error;
use super::io as check_io;
use super::output_root;
use super::run_pair;
use super::trace;

const PREPARATION_CHILD: &str = "ELF_LOADER_MDWE_PREPARATION_CHILD";
const PREPARATION_TARGET: &str = "ELF_LOADER_MDWE_PREPARATION_TARGET";
const PREPARATION_ADDRESS: &str = "ELF_LOADER_MDWE_PREPARATION_ADDRESS";

pub fn mdwe_executable_bss_controls() -> TestResult {
    if env::var_os(PREPARATION_CHILD).is_some() {
        return preparation_in_child();
    }
    mdwe_off_parent()?;
    for interpreter in [false, true] {
        let label = if interpreter { "interpreter" } else { "main" };
        let mut case = Case::new(
            &format!("mdwe-executable-bss-{label}"),
            Fixture::EntryNonPie,
            Naming::Absolute,
            None,
        )?;
        let address = install_executable_bss(&mut case, interpreter, 0x2000)?;
        // Before enabling MDWE, qualify the unmodified full entry and
        // assembly-observer comparisons on this exact executable-BSS ELF.
        let mut pair = run_pair(&case, Mutation::None)?;
        compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
        compare::observers(&pair).map_err(|failure| failure.to_string())?;
        println!("MDWE off {label} executable BSS: full entry and observer parity PASS");

        if !preparation_child(
            &case,
            "mdwe_executable_bss_controls",
            Some((address, interpreter)),
        )? {
            return Ok(());
        }
        case.mdwe = true;
        let native = trace::run(&case, None)?;
        let elapsed = native.elapsed;
        // MDWE does not alter native ELF loading. Compare its entire entry
        // and observer record against the already qualifying loader start
        // with MDWE off, rather than relying only on native exit status.
        pair.native = native;
        compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
        compare::observers(&pair).map_err(|failure| failure.to_string())?;
        println!(
            "MDWE on {label} executable BSS: native full entry and observer control PASS ({:.3}s)",
            elapsed.as_secs_f64()
        );
        // The image and metadata were deliberately prepared with MDWE off.
        // Enabling it only before exec tests the freestanding loader's guard.
        let start = Instant::now();
        let loaded = untraced_loader(&case, &pair.artifacts)?;
        if loaded.status.code() != Some(127)
            || !loaded.stdout.is_empty()
            || loaded.stderr
                != b"reverie-elf-loader: MDWE PR_MDWE_REFUSE_EXEC_GAIN refuses executable BSS -22\n"
        {
            return Err(format!(
                "MDWE on {label} executable BSS escaped loader defensive refusal: {loaded:?}"
            ));
        }
        println!(
            "MDWE on {label} executable BSS: bypassed preparation refused with exit127 and exact diagnostic ({:.3}s)",
            start.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

pub fn mdwe_supported_bss_controls() -> TestResult {
    if env::var_os(PREPARATION_CHILD).is_some() {
        return preparation_in_child();
    }
    mdwe_off_parent()?;
    for same_page in [false, true] {
        let label = if same_page {
            "same-page-executable-bss"
        } else {
            "ordinary-nonexecuting-bss"
        };
        let mut case = Case::new(
            &format!("mdwe-supported-{label}"),
            Fixture::EntryNonPie,
            Naming::Absolute,
            None,
        )?;
        if same_page {
            // RX filesz=14 and memsz=4096 share one page. There are no
            // executable anonymous pages, so this boundary remains admitted.
            install_executable_bss(&mut case, false, 4096)?;
        } else {
            let image = check_io(fs::read(env!("ELF_LOADER_OBSERVER")))?;
            if !header_offsets(&image).into_iter().any(|offset| {
                kind(&image, offset) == 1
                    && flags(&image, offset) & 1 == 0
                    && word(&image, offset + 40) > word(&image, offset + 32)
                    && page_end(word(&image, offset + 16) + word(&image, offset + 40))
                        > page_end(word(&image, offset + 16) + word(&image, offset + 32))
            }) {
                return Err(
                    "ordinary observer has no qualifying nonexecuting anonymous BSS".into(),
                );
            }
        }
        if !preparation_child(&case, "mdwe_supported_bss_controls", None)? {
            return Ok(());
        }
        case.mdwe = true;
        let pair = run_pair(&case, Mutation::None)?;
        compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
        compare::observers(&pair).map_err(|failure| failure.to_string())?;
        println!("MDWE on {label}: preparation, full entry and observer parity PASS");
    }
    Ok(())
}

// SAFETY: callers invoke this only in a fork child's syscall-only path. This
// helper uses no allocation, Rust I/O or lock; errors store only an errno.
pub(super) unsafe fn install_in_child() -> io::Result<()> {
    let result = unsafe {
        libc::prctl(
            libc::PR_SET_MDWE,
            libc::PR_MDWE_REFUSE_EXEC_GAIN as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = unsafe {
        libc::prctl(
            libc::PR_GET_MDWE,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    if flags != libc::PR_MDWE_REFUSE_EXEC_GAIN as libc::c_int {
        // Only PR_SET_MDWE's own EINVAL permits a skip. A failed GET or
        // wrong installed flags must fail even if GET happened to use EINVAL.
        return Err(io::Error::from_raw_os_error(libc::EIO));
    }
    Ok(())
}

fn preparation_child(case: &Case, test: &str, refusal: Option<(u64, bool)>) -> TestResult<bool> {
    let supported = mdwe_off_parent()?;
    let mut command = Command::new(check_io(env::current_exe())?);
    command
        .arg("--exact")
        .arg(test)
        .arg("--nocapture")
        .arg("--test-threads=1")
        .current_dir(&case.root)
        .env(
            PREPARATION_TARGET,
            OsStr::from_bytes(case.invocation.path().to_bytes()),
        );
    if let Some((address, interpreter)) = refusal {
        command
            .env(
                PREPARATION_CHILD,
                if interpreter { "interpreter" } else { "main" },
            )
            .env(PREPARATION_ADDRESS, address.to_string());
    } else {
        command.env(PREPARATION_CHILD, "supported");
    }
    // The fresh test executable gives preparation a normal Rust runtime;
    // preparation itself never runs in an allocator-unsafe post-fork child.
    // SAFETY: the pre_exec callback consists only of the syscall-only helper.
    unsafe {
        command.pre_exec(|| install_in_child());
    }
    let start = Instant::now();
    let output = match command.output() {
        Ok(output) => output,
        Err(error) if !supported && error.raw_os_error() == Some(libc::EINVAL) => {
            println!("SKIP MDWE-on controls: running kernel lacks PR_SET_MDWE (EINVAL)");
            return Ok(false);
        }
        Err(error) => {
            return Err(format!(
                "PR_SET_MDWE/PR_GET_MDWE preparation child setup failed: {error}"
            ));
        }
    };
    check_io(fs::write(
        case.root.join("mdwe-preparation.stdout"),
        &output.stdout,
    ))?;
    check_io(fs::write(
        case.root.join("mdwe-preparation.stderr"),
        &output.stderr,
    ))?;
    if !output.status.success()
        || !output.stderr.is_empty()
        || !output
            .stdout
            .windows(b"MDWE preparation witness PASS".len())
            .any(|bytes| bytes == b"MDWE preparation witness PASS")
    {
        return Err(format!("MDWE preparation subprocess failed: {output:?}"));
    }
    println!(
        "MDWE preparation {}: PASS ({:.3}s)",
        refusal.map_or("supported BSS", |(_, interpreter)| if interpreter {
            "interpreter executable-BSS refusal"
        } else {
            "main executable-BSS refusal"
        }),
        start.elapsed().as_secs_f64()
    );
    Ok(true)
}

fn mdwe_off_parent() -> TestResult<bool> {
    let flags = unsafe {
        libc::prctl(
            libc::PR_GET_MDWE,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    if flags < 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::EINVAL) {
            Ok(false)
        } else {
            Err(format!("PR_GET_MDWE failed in the test parent: {error}"))
        };
    }
    if flags != 0 {
        return Err(format!(
            "MDWE-off control requires an unmodified parent: PR_GET_MDWE={flags:#x}"
        ));
    }
    Ok(true)
}

fn preparation_in_child() -> TestResult {
    let flags = unsafe {
        libc::prctl(
            libc::PR_GET_MDWE,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    if flags != libc::PR_MDWE_REFUSE_EXEC_GAIN as libc::c_int {
        return Err(format!(
            "preparation subprocess did not inherit PR_MDWE_REFUSE_EXEC_GAIN: PR_GET_MDWE={flags}"
        ));
    }
    let path = env::var_os(PREPARATION_TARGET).ok_or("preparation target environment missing")?;
    let target = check_io(File::open(&path))?;
    let invocation = error(Invocation::execve(&path))?;
    let limits = error(Limits::current())?;
    let result = prepare_start_with_limits(&target, &invocation, Path::new("./h"), limits);
    let mode = env::var(PREPARATION_CHILD).map_err(|error| error.to_string())?;
    match mode.as_str() {
        "supported" => {
            error(result)?;
        }
        "main" | "interpreter" => {
            let expected = env::var(PREPARATION_ADDRESS)
                .map_err(|error| error.to_string())?
                .parse::<u64>()
                .map_err(|error| error.to_string())?;
            match result {
                Err(Error::MdweExecutableBss {
                    address,
                    interpreter,
                }) if address == expected && interpreter == (mode == "interpreter") => {
                    let diagnostic = Error::MdweExecutableBss {
                        address,
                        interpreter,
                    }
                    .to_string();
                    if !diagnostic.contains("MDWE")
                        || !diagnostic.contains("PR_MDWE_REFUSE_EXEC_GAIN")
                    {
                        return Err(format!("executable-BSS refusal omits MDWE: {diagnostic}"));
                    }
                    println!("named preparation refusal: {diagnostic}");
                }
                result => {
                    return Err(format!(
                        "wrong MDWE {mode} executable-BSS preparation result at {expected:#x}: {result:?}"
                    ));
                }
            }
        }
        mode => return Err(format!("unknown MDWE preparation child mode {mode:?}")),
    }
    println!("MDWE preparation witness PASS");
    Ok(())
}

fn install_executable_bss(case: &mut Case, interpreter: bool, memsz: u64) -> TestResult<u64> {
    let mut main = check_io(fs::read(Fixture::EntryNonPie.path()))?;
    let mut observer = check_io(fs::read(env!("ELF_LOADER_OBSERVER")))?;
    let image = if interpreter {
        &mut observer
    } else {
        &mut main
    };
    let offset = header_offsets(image)
        .into_iter()
        .find(|offset| kind(image, *offset) == 1 && flags(image, *offset) == 5)
        .ok_or("entry fixture has no RX PT_LOAD")?;
    let address = word(image, offset + 16);
    let filesz = word(image, offset + 32);
    if address & 4095 != 0 || filesz == 0 || filesz >= 4096 || memsz <= filesz {
        return Err("executable-BSS control no longer has independent one-page RX geometry".into());
    }
    if !interpreter && (address != 0x401000 || filesz != 14) {
        return Err(format!(
            "review reproducer changed: RX address={address:#x}, filesz={filesz}"
        ));
    }
    put_word(image, offset + 40, memsz);
    if interpreter {
        let path = case.root.join("bss-interpreter");
        write_fixture(
            &path,
            &observer,
            "observer RX PT_LOAD memsz extended to0x2000",
        )?;
        set_interpreter(&mut main, &path)?;
    }
    let path = case.root.join("target");
    write_fixture(
        &path,
        &main,
        if interpreter {
            "entry-nonpie PT_INTERP names executable-BSS observer"
        } else if memsz == 4096 {
            "entry-nonpie RX PT_LOAD filesz14, same-page memsz4096"
        } else {
            "entry-nonpie RX PT_LOAD filesz14, memsz0x2000 (review reproducer)"
        },
    )?;
    case.target = check_io(File::open(&path))?;
    case.invocation = error(Invocation::execve(path.as_os_str()))?;
    Ok(address)
}

fn untraced_loader(case: &Case, artifacts: &Artifacts) -> TestResult<Output> {
    let path = artifacts.prepared.padded_path.as_c_str();
    let mut command = Command::new(OsStr::from_bytes(path.to_bytes()));
    command.arg0(OsStr::from_bytes(case.argv[0].as_bytes()));
    for arg in &case.argv[1..] {
        command.arg(OsStr::from_bytes(arg.as_bytes()));
    }
    command.current_dir(&case.root).env_clear();
    for variable in &case.env {
        let bytes = variable.as_bytes();
        let separator = bytes
            .iter()
            .position(|byte| *byte == b'=')
            .expect("fixture variable contains '='");
        command.env(
            OsStr::from_bytes(&bytes[..separator]),
            OsStr::from_bytes(&bytes[separator + 1..]),
        );
    }
    let target_fd = case.target.as_raw_fd();
    let metadata_fd = artifacts.metadata.as_raw_fd();
    let data_limit = case.limits.data;
    let mdwe = case.mdwe;
    // SAFETY: values are integers prepared before fork. The callback only
    // calls syscall wrappers and the syscall-only MDWE helper.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(target_fd, TARGET_FD) < 0
                || libc::fcntl(TARGET_FD, libc::F_SETFD, 0) < 0
                || libc::dup2(metadata_fd, METADATA_FD) < 0
                || libc::fcntl(METADATA_FD, libc::F_SETFD, 0) < 0
            {
                return Err(io::Error::last_os_error());
            }
            let current = libc::personality(!0 as libc::c_ulong);
            if current < 0
                || libc::personality(
                    current as libc::c_ulong | libc::ADDR_NO_RANDOMIZE as libc::c_ulong,
                ) < 0
                || libc::personality(!0 as libc::c_ulong) & libc::ADDR_NO_RANDOMIZE == 0
            {
                return Err(io::Error::last_os_error());
            }
            let data = libc::rlimit {
                rlim_cur: data_limit,
                rlim_max: data_limit,
            };
            let core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_DATA, &data) < 0
                || libc::setrlimit(libc::RLIMIT_CORE, &core) < 0
            {
                return Err(io::Error::last_os_error());
            }
            if mdwe {
                install_in_child()?;
            }
            libc::alarm(30);
            Ok(())
        });
    }
    let output = check_io(command.output())?;
    check_io(fs::write(
        case.root.join("mdwe-loader.stdout"),
        &output.stdout,
    ))?;
    check_io(fs::write(
        case.root.join("mdwe-loader.stderr"),
        &output.stderr,
    ))?;
    Ok(output)
}

fn write_fixture(path: &Path, image: &[u8], operation: &str) -> TestResult {
    check_io(fs::write(path, image))?;
    check_io(fs::set_permissions(path, Permissions::from_mode(0o755)))?;
    let hash = check_io(Command::new("sha256sum").arg(path).output())?;
    if !hash.status.success() {
        return Err("sha256sum failed for MDWE ELF fixture".into());
    }
    let mut evidence = check_io(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(output_root()?.join("mdwe-images.provenance")),
    )?;
    check_io(writeln!(
        evidence,
        "operation: {operation}\nsha256: {}",
        String::from_utf8_lossy(&hash.stdout).trim_end()
    ))
}

fn set_interpreter(image: &mut Vec<u8>, path: &Path) -> TestResult {
    let offset = header_offsets(image)
        .into_iter()
        .find(|offset| kind(image, *offset) == 3)
        .ok_or("entry fixture has no PT_INTERP")?;
    let start = image.len() as u64;
    let name = path.as_os_str().as_bytes();
    image.extend_from_slice(name);
    image.push(0);
    put_word(image, offset + 8, start);
    put_word(image, offset + 32, name.len() as u64 + 1);
    put_word(image, offset + 40, name.len() as u64 + 1);
    Ok(())
}

fn page_end(value: u64) -> u64 {
    (value + 4095) & !4095
}

fn header_offsets(image: &[u8]) -> Vec<usize> {
    assert_eq!(u16::from_le_bytes(image[54..56].try_into().unwrap()), 56);
    let count = u16::from_le_bytes(image[56..58].try_into().unwrap()) as usize;
    let start = word(image, 32) as usize;
    (0..count).map(|index| start + index * 56).collect()
}

fn kind(image: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(image[offset..offset + 4].try_into().unwrap())
}

fn flags(image: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(image[offset + 4..offset + 8].try_into().unwrap())
}

fn word(image: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(image[offset..offset + 8].try_into().unwrap())
}

fn put_word(image: &mut [u8], offset: usize, value: u64) {
    image[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
