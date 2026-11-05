/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;

use reverie_elf_loader::Error;
use reverie_elf_loader::prepare_start_with_limits;

use super::Case;
use super::Fixture;
use super::Mutation;
use super::Naming;
use super::TestResult;
use super::compare;
use super::geometry::header_offsets;
use super::geometry::install_main;
use super::geometry::kind;
use super::geometry::word;
use super::geometry::write_fixture;
use super::io as check_io;
use super::run_pair;

const MAIN_ADDRESS: u64 = 0x400000;
const ENTRY_OFFSET: u64 = 0x180;
const LOAD_SIZE: u64 = 0x200;

pub fn empty_last_load_zero_data_controls() -> TestResult {
    for address in [0x900123_u64, 0x900000] {
        for limit in [4096_u64, 0] {
            let test = format!("empty-last-load-{address:x}-data-{limit}");
            let mut case = Case::new(&test, Fixture::EntryNonPie, Naming::Absolute, Some(limit))?;
            let interpreter = case.root.join("rx-only-interpreter");
            write_fixture(
                &interpreter,
                &rx_interpreter()?,
                "one RX PT_LOAD ET_DYN interpreter; exit_group(0); no writable data or BSS",
            )?;
            let main = main_image(&interpreter, address, 0)?;
            install_main(&mut case, &main)?;
            let pair = run_pair(&case, Mutation::None)?;
            let rounded_end = (address + 4095) & !4095;
            let native = &pair.native.entry;
            if native.stat[&45] != address
                || native.stat[&46] != address
                || native.stat[&47] != rounded_end
                || aux(&native.aux, 3)? != MAIN_ADDRESS + 64
                || aux(&native.aux, 5)? != 4
                || aux(&native.aux, 9)? != MAIN_ADDRESS + ENTRY_OFFSET
                || pair.native.peak_data_kib != 0
            {
                return Err("RX-only native control did not witness exact empty-load metadata and zero private data".into());
            }
            println!(
                "{test}: RLIMIT_DATA cur=max={limit}, native start_data=end_data=memory_end={address:#x}, start_brk={rounded_end:#x}, native peak=0KiB, loader peak={}KiB",
                pair.loaded.peak_data_kib
            );
            // The original comparator must reject an invented shadow BSS page.
            // All existing entry/stack/maps/register/auxv checks still run.
            compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
            if pair.loaded.peak_data_kib != 0
                || pair.artifacts.prepared.layout.minimum_data_limit != 0
                || pair.artifacts.prepared.layout.start_data != address
                || pair.artifacts.prepared.layout.end_data != address
                || pair.artifacts.prepared.layout.memory_end != address
                || pair.artifacts.prepared.layout.start_brk != rounded_end
            {
                return Err("empty-load zero-data control changed its independently fixed footprint or metadata".into());
            }
            let image = check_io(fs::read(&pair.artifacts.image))?;
            let shadow = header_offsets(&image)
                .into_iter()
                .find(|at| kind(&image, *at) == 1 && word(&image, *at + 16) == address)
                .ok_or("empty-load control has no matching shadow PT_LOAD")?;
            if kind(&image, shadow + 4) != 4
                || word(&image, shadow + 32) != 0
                || word(&image, shadow + 40) != 0
                || word(&image, shadow + 8) % 4096 != address % 4096
            {
                return Err("empty-load shadow must be R-only with zero filesz/memsz and a congruent offset".into());
            }
            println!("{test}: raw shadow filesz=memsz=0; full native/loader entry parity PASS");
        }
    }
    real_bss_boundary()
}

fn real_bss_boundary() -> TestResult {
    let mut case = Case::new(
        "empty-last-load-real-bss-boundary",
        Fixture::EntryNonPie,
        Naming::Absolute,
        Some(4096),
    )?;
    let interpreter = case.root.join("rx-only-interpreter");
    write_fixture(
        &interpreter,
        &rx_interpreter()?,
        "one RX PT_LOAD ET_DYN interpreter; exit_group(0); no writable data or BSS",
    )?;
    let address = 0x900123;
    install_main(&mut case, &main_image(&interpreter, address, 1)?)?;
    let pair = run_pair(&case, Mutation::None)?;
    compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
    if pair.artifacts.prepared.layout.minimum_data_limit != 4096
        || pair.artifacts.prepared.layout.memory_end != address + 1
        || pair.native.peak_data_kib != 4
        || pair.loaded.peak_data_kib != 4
        || pair.native.entry.stat[&45] != address
        || pair.native.entry.stat[&46] != address
        || pair.native.entry.stat[&47] != 0x901000
    {
        return Err(
            "real one-byte BSS control did not require exactly one page of private data".into(),
        );
    }
    let image = check_io(fs::read(&pair.artifacts.image))?;
    let shadow = header_offsets(&image)
        .into_iter()
        .find(|at| kind(&image, *at) == 1 && word(&image, *at + 16) == address)
        .ok_or("real-BSS control has no matching shadow PT_LOAD")?;
    if word(&image, shadow + 32) != 0 || word(&image, shadow + 40) != 1 {
        return Err("real-BSS shadow must preserve raw one-byte memory size".into());
    }
    case.limits.data = 1;
    match prepare_start_with_limits(
        &case.target,
        &case.invocation,
        Path::new("./h"),
        case.limits,
    ) {
        Err(Error::FiniteDataLimitBelowStartupFootprint {
            limit: 1,
            minimum: 4096,
        }) => {}
        result => {
            return Err(format!(
                "real BSS below startup boundary gave wrong refusal: {result:?}"
            ));
        }
    }
    let mut command = Command::new(OsStr::from_bytes(case.invocation.path().to_bytes()));
    command
        .current_dir(&case.root)
        .arg0(OsStr::from_bytes(case.argv[0].as_bytes()))
        .args(
            case.argv[1..]
                .iter()
                .map(|value| OsStr::from_bytes(value.as_bytes())),
        )
        .env_clear();
    for value in &case.env {
        let bytes = value.as_bytes();
        let separator = bytes.iter().position(|byte| *byte == b'=').unwrap();
        command.env(
            OsStr::from_bytes(&bytes[..separator]),
            OsStr::from_bytes(&bytes[separator + 1..]),
        );
    }
    // SAFETY: only syscall wrappers run after fork; no allocation or locks.
    unsafe {
        command.pre_exec(|| {
            let current = libc::personality(!0 as libc::c_ulong);
            if current < 0
                || libc::personality(
                    current as libc::c_ulong | libc::ADDR_NO_RANDOMIZE as libc::c_ulong,
                ) < 0
            {
                return Err(io::Error::last_os_error());
            }
            let data = libc::rlimit {
                rlim_cur: 1,
                rlim_max: 1,
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
            Ok(())
        });
    }
    let native = check_io(command.output())?;
    check_io(fs::write(
        case.root.join("below-boundary-native.stdout"),
        &native.stdout,
    ))?;
    check_io(fs::write(
        case.root.join("below-boundary-native.stderr"),
        &native.stderr,
    ))?;
    if native.status.signal() != Some(libc::SIGSEGV)
        || !native.stdout.is_empty()
        || !native.stderr.is_empty()
    {
        return Err(format!(
            "real BSS native below-boundary startup must die SIGSEGV without output: {native:?}"
        ));
    }
    println!(
        "one-byte real BSS: DATAcur=max4096 full parity and peak4KiB; DATAcur=max1 nativeSIGSEGV and exact named minimum4096 refusal"
    );
    Ok(())
}

fn elf_header(elf_type: u16, count: u16, entry: u64) -> TestResult<Vec<u8>> {
    let source = check_io(fs::read(env!("ELF_LOADER_ENTRY_NONPIE")))?;
    let mut image = source[..64].to_vec();
    image[16..18].copy_from_slice(&elf_type.to_le_bytes());
    put_word(&mut image, 24, entry);
    put_word(&mut image, 32, 64);
    put_word(&mut image, 40, 0);
    image[56..58].copy_from_slice(&count.to_le_bytes());
    image[58..64].fill(0);
    image.resize(LOAD_SIZE as usize, 0);
    Ok(image)
}

fn rx_interpreter() -> TestResult<Vec<u8>> {
    let mut image = elf_header(3, 1, ENTRY_OFFSET)?;
    program_header(&mut image, 0, 1, 5, [0, 0, 0, LOAD_SIZE, LOAD_SIZE, 4096]);
    exit_code(&mut image);
    Ok(image)
}

fn main_image(interpreter: &Path, address: u64, memsz: u64) -> TestResult<Vec<u8>> {
    let mut image = elf_header(2, 4, MAIN_ADDRESS + ENTRY_OFFSET)?;
    program_header(
        &mut image,
        0,
        1,
        5,
        [0, MAIN_ADDRESS, MAIN_ADDRESS, LOAD_SIZE, LOAD_SIZE, 4096],
    );
    program_header(
        &mut image,
        1,
        1,
        4,
        [address % 4096, address, address, 0, memsz, 4096],
    );
    let name = interpreter.as_os_str().as_bytes();
    program_header(
        &mut image,
        2,
        3,
        4,
        [
            LOAD_SIZE,
            0,
            0,
            name.len() as u64 + 1,
            name.len() as u64 + 1,
            1,
        ],
    );
    program_header(&mut image, 3, 0x6474e551, 6, [0, 0, 0, 0, 0, 16]);
    exit_code(&mut image);
    image.extend_from_slice(name);
    image.push(0);
    Ok(image)
}

fn program_header(image: &mut [u8], index: usize, kind: u32, flags: u32, words: [u64; 6]) {
    let at = 64 + index * 56;
    image[at..at + 4].copy_from_slice(&kind.to_le_bytes());
    image[at + 4..at + 8].copy_from_slice(&flags.to_le_bytes());
    for (index, value) in words.into_iter().enumerate() {
        put_word(image, at + 8 + index * 8, value);
    }
}

fn exit_code(image: &mut [u8]) {
    // mov $231,%eax; xor %edi,%edi; syscall -- no data/BSS or stack stores.
    let code = [0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05];
    image[ENTRY_OFFSET as usize..ENTRY_OFFSET as usize + code.len()].copy_from_slice(&code);
}

fn put_word(image: &mut [u8], offset: usize, value: u64) {
    image[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn aux(values: &[(u64, u64)], requested: u64) -> TestResult<u64> {
    values
        .iter()
        .find_map(|&(kind, value)| (kind == requested).then_some(value))
        .ok_or_else(|| format!("auxv type {requested} missing"))
}
