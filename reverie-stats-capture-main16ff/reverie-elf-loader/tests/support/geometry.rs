/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

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
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;
use std::process::Output;
use std::time::Instant;

use reverie_elf_loader::Error;
use reverie_elf_loader::Invocation;
use reverie_elf_loader::MIN_PROGRAM_ADDRESS;
use reverie_elf_loader::prepare_start_with_limits;

use super::Artifacts;
use super::Case;
use super::Fixture;
use super::METADATA_FD;
use super::Mutation;
use super::Naming;
use super::Pair;
use super::TARGET_FD;
use super::TestResult;
use super::compare;
use super::error;
use super::io as check_io;
use super::output_root;
use super::run_pair;
use super::trace;

const INTERPRETER_HINT: u64 = 0x800000;
const SECOND_HEADER_PAGE: u64 = 0x900000;
const HIGH_MAIN_ADDRESS: u64 = 0x1000000;

#[derive(Clone, Copy)]
struct InterpreterPlacement {
    bias: u64,
    first_mapping: u64,
    first_file: u64,
}

pub fn unaligned_pie_alignment(
    test: &str,
    second_alignment: Option<u64>,
    expected_bias: u64,
) -> TestResult {
    let mut case = Case::new(test, Fixture::EntryPie, Naming::Absolute, None)?;
    let image = unaligned_pie_image(second_alignment)?;
    install_main(&mut case, &image)?;
    let pair = run_pair(&case, Mutation::None)?;
    let last_address = if second_alignment.is_some() {
        0x2123
    } else {
        0x123
    };
    let expected_brk = expected_bias
        + if second_alignment.is_some() {
            0x3000
        } else {
            0x1000
        };
    let path = case.root.join("target");
    if aux(&pair.native.entry.aux, 3)? != expected_bias + 0x123
        || aux(&pair.native.entry.aux, 9)? != expected_bias + 0x123
        || pair.native.entry.stat[&45] != expected_bias + last_address
        || pair.native.entry.stat[&46] != expected_bias + last_address + 0x200
        || pair.native.entry.stat[&47] != expected_brk
        || native_load_mappings(&pair.native, &image, expected_bias, &path)? != Some(expected_bias)
    {
        return Err(format!(
            "native unaligned PIE did not witness independently expected bias {expected_bias:#x} and exact auxv/data/brk/mapping geometry"
        ));
    }
    native_observer_record(&pair.native)?;
    save_native_geometry_evidence(&case, &pair.native, "unaligned-pie")?;
    println!(
        "{test}: first offset=vaddr=0x123, first align=0, second align={second_alignment:#x?}; native bias={expected_bias:#x}, AT_PHDR=AT_ENTRY={:#x}, start_data={:#x}, end_data={:#x}, start_brk={expected_brk:#x}",
        expected_bias + 0x123,
        expected_bias + last_address,
        expected_bias + last_address + 0x200,
    );
    // Keep the ordinary full-entry and independent observer comparators.
    // The native witness above does not depend on preparation's calculation.
    compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
    compare::observers(&pair).map_err(|failure| failure.to_string())?;
    if pair.artifacts.prepared.layout.load_bias != expected_bias {
        return Err(format!(
            "prepared unaligned PIE bias did not match the independent native bias {expected_bias:#x}"
        ));
    }
    Ok(())
}

fn unaligned_pie_image(second_alignment: Option<u64>) -> TestResult<Vec<u8>> {
    let source = check_io(fs::read(Fixture::EntryPie.path()))?;
    let mut image = source[..64].to_vec();
    let load_count = if second_alignment.is_some() { 2 } else { 1 };
    image[16..18].copy_from_slice(&3_u16.to_le_bytes());
    put_word(&mut image, 24, 0x123);
    put_word(&mut image, 32, 0x123);
    put_word(&mut image, 40, 0);
    image[56..58].copy_from_slice(&4_u16.to_le_bytes());
    image[58..64].fill(0);
    image.resize(if load_count == 2 { 0x2400 } else { 0x400 }, 0);
    for (index, (address, flags, alignment)) in [
        (0x123, 5_u32, 0),
        (0x2123, 6, second_alignment.unwrap_or(0)),
    ]
    .into_iter()
    .take(load_count)
    .enumerate()
    {
        let at = 0x123 + index * 56;
        image[at..at + 4].copy_from_slice(&1_u32.to_le_bytes());
        image[at + 4..at + 8].copy_from_slice(&flags.to_le_bytes());
        for (field, value) in [
            (8, address),
            (16, address),
            (24, address),
            (32, 0x200),
            (40, 0x200),
            (48, alignment),
        ] {
            put_word(&mut image, at + field, value);
        }
    }
    let name = env!("ELF_LOADER_OBSERVER").as_bytes();
    let interp = 0x123 + load_count * 56;
    let offset = image.len() as u64;
    image[interp..interp + 4].copy_from_slice(&3_u32.to_le_bytes());
    put_word(&mut image, interp + 8, offset);
    put_word(&mut image, interp + 32, name.len() as u64 + 1);
    put_word(&mut image, interp + 40, name.len() as u64 + 1);
    image.extend_from_slice(name);
    image.push(0);
    let stack = interp + 56;
    image[stack..stack + 4].copy_from_slice(&0x6474e551_u32.to_le_bytes());
    image[stack + 4..stack + 8].copy_from_slice(&6_u32.to_le_bytes());
    put_word(&mut image, stack + 48, 16);
    if load_count == 1 {
        // Keep AT_PHNUM distinct from the loader's three headers, so this
        // control uses the unchanged ordinary exact-difference comparator.
        image[stack + 56..stack + 60].copy_from_slice(&4_u32.to_le_bytes());
    }
    Ok(image)
}

pub fn interpreter_address_hint(test: &str, fixture: Fixture) -> TestResult {
    let mut case = Case::new(test, fixture, Naming::Absolute, None)?;
    let mut interpreter = check_io(fs::read(env!("ELF_LOADER_OBSERVER")))?;
    let entry = word(&interpreter, 24) + INTERPRETER_HINT;
    put_word(&mut interpreter, 24, entry);
    for offset in header_offsets(&interpreter) {
        if kind(&interpreter, offset) == 1 {
            let address = word(&interpreter, offset + 16) + INTERPRETER_HINT;
            put_word(&mut interpreter, offset + 16, address);
            put_word(&mut interpreter, offset + 24, address);
        }
    }
    let path = case.root.join("hint-interpreter");
    write_fixture(
        &path,
        &interpreter,
        "observer PT_LOADs and entry shifted by 0x800000",
    )?;
    let mut main = check_io(fs::read(fixture.path()))?;
    set_interpreter(&mut main, &path);
    install_main(&mut case, &main)?;
    let pair = run_pair(&case, Mutation::None)?;
    let base = aux(&pair.native.entry.aux, 7)?;
    if matches!(fixture, Fixture::EntryNonPie) {
        if pair.artifacts.prepared.layout.load_bias != 0
            || base != 0
            || pair.native.entry.registers.rip != entry
        {
            return Err(format!(
                "ET_EXEC main did not honor free interpreter hint {INTERPRETER_HINT:#x}: AT_BASE={base:#x}, RIP={:#x}",
                pair.native.entry.registers.rip
            ));
        }
        compare::entries_with_zero_interpreter_bias(&case, &pair)
            .map_err(|failure| failure.to_string())?;
    } else {
        if pair.artifacts.prepared.layout.load_bias == 0 || base == 0 {
            return Err("PIE control did not discard the interpreter hint".into());
        }
        compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
    }
    if pair.loaded.entry.registers.rip != entry + base {
        return Err("loader interpreter entry did not match the native hint/bias".into());
    }
    compare::observers(&pair).map_err(|failure| failure.to_string())?;
    println!(
        "{test}: first PT_LOAD hint={INTERPRETER_HINT:#x}, main bias={:#x}, native/loader AT_BASE={base:#x}, RIP={:#x}",
        pair.artifacts.prepared.layout.load_bias, pair.native.entry.registers.rip
    );
    Ok(())
}

pub fn interpreter_hint_reserved_range() -> TestResult {
    let mut case = Case::new(
        "interpreter-hint-reserved-range",
        Fixture::EntryNonPie,
        Naming::Absolute,
        None,
    )?;
    let interpreter_path = case.root.join("boundary-interpreter");
    let mut main = check_io(fs::read(Fixture::EntryNonPie.path()))?;
    let first = header_offsets(&main)
        .into_iter()
        .find(|offset| kind(&main, *offset) == 1)
        .expect("entry fixture has PT_LOAD");
    let relocation = HIGH_MAIN_ADDRESS - (word(&main, first + 16) & !4095);
    let entry = word(&main, 24) + relocation;
    put_word(&mut main, 24, entry);
    for offset in header_offsets(&main) {
        for field in [16, 24] {
            let address = word(&main, offset + field);
            if address != 0 {
                put_word(&mut main, offset + field, address + relocation);
            }
        }
    }
    set_interpreter(&mut main, &interpreter_path);
    install_main(&mut case, &main)?;

    let interpreter = interpreter_hint_image(MIN_PROGRAM_ADDRESS)?;
    write_fixture(
        &interpreter_path,
        &interpreter,
        "ET_DYN observer first PT_LOAD hint=0x400000; ET_EXEC main moved to 0x1000000",
    )?;
    let pair = run_pair(&case, Mutation::None)?;
    if pair.artifacts.prepared.layout.load_bias != 0
        || pair.artifacts.prepared.layout.program_base != HIGH_MAIN_ADDRESS
    {
        return Err("interpreter hint boundary did not use the relocated ET_EXEC main".into());
    }
    native_free_interpreter_hint(&case, &pair.native, &interpreter, &interpreter_path)?;
    compare::entries_with_zero_interpreter_bias(&case, &pair)
        .map_err(|failure| failure.to_string())?;
    compare::observers(&pair).map_err(|failure| failure.to_string())?;
    let image_name = pair
        .artifacts
        .image
        .to_str()
        .ok_or("qualifying loader pathname is not UTF-8")?;
    let loaded_maps =
        std::str::from_utf8(&pair.loaded.entry.maps).map_err(|error| error.to_string())?;
    if !loaded_maps.lines().any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        fields.get(1) == Some(&"r-xp")
            && fields.get(5) == Some(&image_name)
            && fields
                .first()
                .and_then(|range| range.split_once('-'))
                .is_some_and(|(start, _)| u64::from_str_radix(start, 16) == Ok(0x100000))
    }) {
        return Err(
            "qualifying loader lacks its independently witnessed RX mapping at 0x100000".into(),
        );
    }
    check_io(fs::write(
        case.root.join("qualified-loader.maps"),
        &pair.loaded.entry.maps,
    ))?;
    println!("interpreter hint 0x400000: full native/loader entry and observer parity PASS");

    // Reuse this fully qualified loader image and metadata. Only the pinned
    // interpreter pathname's contents change, so these starts independently
    // exercise the loader's check after the host preparation boundary.
    for hint in [MIN_PROGRAM_ADDRESS - 4096, 0x100000] {
        let interpreter = interpreter_hint_image(hint)?;
        let span = interpreter_mapping_span(&interpreter);
        write_fixture(
            &interpreter_path,
            &interpreter,
            &format!(
                "ET_DYN observer first PT_LOAD hint={hint:#x}; ET_EXEC main remains at 0x1000000"
            ),
        )?;
        match prepare_start_with_limits(
            &case.target,
            &case.invocation,
            Path::new("./h"),
            case.limits,
        ) {
            Err(Error::InterpreterHintOverlapsLoader {
                address,
                span: observed,
            }) if address == hint && observed == span => {}
            result => {
                return Err(format!(
                    "reserved interpreter hint {hint:#x} gave wrong preparation refusal: {result:?}"
                ));
            }
        }
        let native = trace::run(&case, None)?;
        native_free_interpreter_hint(&case, &native, &interpreter, &interpreter_path)?;
        let loaded = untraced_start(
            &case,
            Some(&pair.artifacts),
            &format!("hint-{hint:x}-loader"),
        )?;
        if loaded.status.code() != Some(127)
            || !loaded.stdout.is_empty()
            || loaded.stderr
                != b"reverie-elf-loader: interpreter PT_LOAD hint overlaps loader reserved range -22\n"
        {
            return Err(format!(
                "reserved interpreter hint {hint:#x} escaped the loader guard: {loaded:?}"
            ));
        }
        println!(
            "interpreter hint {hint:#x}: independently free native span, AT_BASE=0, exact hinted RIP/maps; host InterpreterHintOverlapsLoader; qualified loader exit127 with exact diagnostic"
        );
    }
    Ok(())
}

pub fn interpreter_hint_exceptions() -> TestResult {
    for (fixture, hint, label) in [
        (Fixture::EntryNonPie, 0, "zero-hint-et-exec"),
        (Fixture::EntryPie, 0x100000, "reserved-hint-pie"),
    ] {
        let mut case = Case::new(
            &format!("interpreter-hint-{label}"),
            fixture,
            Naming::Absolute,
            None,
        )?;
        let interpreter = interpreter_hint_image(hint)?;
        let interpreter_path = case.root.join("control-interpreter");
        write_fixture(
            &interpreter_path,
            &interpreter,
            &format!("ET_DYN observer first PT_LOAD hint={hint:#x}; {label} control"),
        )?;
        let mut main = check_io(fs::read(fixture.path()))?;
        set_interpreter(&mut main, &interpreter_path);
        install_main(&mut case, &main)?;
        let pair = run_pair(&case, Mutation::None)?;
        let base = aux(&pair.native.entry.aux, 7)?;
        let main_is_biased = pair.artifacts.prepared.layout.load_bias != 0;
        if main_is_biased != matches!(fixture, Fixture::EntryPie)
            || base == 0
            || pair.native.entry.registers.rip != base + word(&interpreter, 24)
        {
            return Err(format!(
                "{label} did not qualify the ordinary unbiased/biased hint behavior: AT_BASE={base:#x}, RIP={:#x}",
                pair.native.entry.registers.rip
            ));
        }
        compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
        compare::observers(&pair).map_err(|failure| failure.to_string())?;
        println!(
            "{label}: hint={hint:#x}, main bias={:#x}, native/loader AT_BASE={base:#x}; full entry and observer parity PASS",
            pair.artifacts.prepared.layout.load_bias
        );
    }
    Ok(())
}

pub fn interpreter_mixed_empty_reserved_range() -> TestResult {
    for fixture in [Fixture::EntryNonPie, Fixture::EntryPie] {
        for empty_address in [0, 0x123] {
            let mut case = Case::new(
                &format!("interpreter-mixed-empty-{empty_address:x}"),
                fixture,
                Naming::Absolute,
                None,
            )?;
            let interpreter_path = case.root.join("mixed-interpreter");
            install_interpreter_control_main(&mut case, fixture, &interpreter_path)?;
            let interpreter = mixed_empty_interpreter_image(MIN_PROGRAM_ADDRESS, empty_address)?;
            write_fixture(
                &interpreter_path,
                &interpreter,
                &format!(
                    "ET_DYN observer first PT_LOAD filesz=memsz=0 at {empty_address:#x}; first nonempty PT_LOAD at 0x400000"
                ),
            )?;
            let pair = run_pair(&case, Mutation::None)?;
            qualify_zero_bias_interpreter(
                &case,
                fixture,
                &pair,
                &interpreter,
                &interpreter_path,
                MIN_PROGRAM_ADDRESS,
                MIN_PROGRAM_ADDRESS,
            )?;
            println!(
                "mixed empty/nonempty {} first={empty_address:#x}: 0x400000 full native/loader entry and independent observer parity PASS",
                fixture.label()
            );

            // Keep the passing start image, main, metadata and invocation.
            // Only interpreter contents change across this preparation bypass.
            for address in [MIN_PROGRAM_ADDRESS - 4096, 0x100000] {
                let interpreter = mixed_empty_interpreter_image(address, empty_address)?;
                write_fixture(
                    &interpreter_path,
                    &interpreter,
                    &format!(
                        "ET_DYN observer first PT_LOAD remains empty at {empty_address:#x}; first nonempty PT_LOAD moved to {address:#x}"
                    ),
                )?;
                refuse_zero_bias_interpreter(
                    &case,
                    &pair.artifacts,
                    &interpreter,
                    &interpreter_path,
                    address,
                    address,
                )?;
            }
        }
    }
    Ok(())
}

pub fn interpreter_empty_bss_reserved_range() -> TestResult {
    for fixture in [Fixture::EntryNonPie, Fixture::EntryPie] {
        for (elf_type, label) in [(3, "empty-then-bss"), (2, "first-bss")] {
            let mut case = Case::new(
                &format!("interpreter-{label}"),
                fixture,
                Naming::Absolute,
                None,
            )?;
            let interpreter_path = case.root.join("bss-interpreter");
            install_interpreter_control_main(&mut case, fixture, &interpreter_path)?;
            let interpreter = empty_bss_interpreter_image(MIN_PROGRAM_ADDRESS, elf_type)?;
            write_fixture(
                &interpreter_path,
                &interpreter,
                &format!(
                    "{label} observer: filesz=0 memsz=4096 PT_LOAD at 0x400000; observer text at 0x800000"
                ),
            )?;
            let pair = run_pair(&case, Mutation::None)?;
            qualify_zero_bias_interpreter(
                &case,
                fixture,
                &pair,
                &interpreter,
                &interpreter_path,
                MIN_PROGRAM_ADDRESS,
                INTERPRETER_HINT,
            )?;
            println!(
                "{label} {}: 0x400000 anonymous BSS, full native/loader entry and independent observer parity PASS",
                fixture.label()
            );

            for address in [MIN_PROGRAM_ADDRESS - 4096, 0x100000] {
                let interpreter = empty_bss_interpreter_image(address, elf_type)?;
                write_fixture(
                    &interpreter_path,
                    &interpreter,
                    &format!(
                        "{label} observer: only filesz=0 memsz=4096 PT_LOAD moved to {address:#x}; text remains at 0x800000"
                    ),
                )?;
                refuse_zero_bias_interpreter(
                    &case,
                    &pair.artifacts,
                    &interpreter,
                    &interpreter_path,
                    address,
                    INTERPRETER_HINT,
                )?;
            }
        }
    }
    Ok(())
}

pub fn interpreter_empty_negative_bias_reserved_range() -> TestResult {
    let raw_bias = INTERPRETER_HINT;
    let expected_bias = 0u64.wrapping_sub(raw_bias);
    let mut case = Case::new(
        "interpreter-empty-negative-bias",
        Fixture::EntryPie,
        Naming::Absolute,
        None,
    )?;
    let interpreter_path = case.root.join("negative-bias-interpreter");
    install_interpreter_control_main(&mut case, Fixture::EntryPie, &interpreter_path)?;
    let interpreter =
        mixed_empty_interpreter_image(MIN_PROGRAM_ADDRESS + raw_bias, raw_bias + 0x123)?;
    write_fixture(
        &interpreter_path,
        &interpreter,
        "ET_DYN observer first PT_LOAD empty at 0x800123; raw first nonempty load=0xc00000, effective=0x400000 under PIE",
    )?;
    let pair = run_pair(&case, Mutation::None)?;
    qualify_effective_interpreter(
        &case,
        Fixture::EntryPie,
        &pair,
        &interpreter,
        &interpreter_path,
        InterpreterPlacement {
            bias: expected_bias,
            first_mapping: MIN_PROGRAM_ADDRESS,
            first_file: MIN_PROGRAM_ADDRESS,
        },
    )?;
    println!(
        "PIE empty-first negative interpreter bias: AT_BASE={expected_bias:#x}, 0x400000 full native/loader entry and independent observer parity PASS"
    );

    for address in [MIN_PROGRAM_ADDRESS - 4096, 0x100000] {
        let interpreter = mixed_empty_interpreter_image(address + raw_bias, raw_bias + 0x123)?;
        write_fixture(
            &interpreter_path,
            &interpreter,
            &format!(
                "PIE empty-first observer: first empty load stays at 0x800123; raw first nonempty={:#x}, effective={address:#x}",
                address + raw_bias
            ),
        )?;
        refuse_effective_interpreter(
            &case,
            &pair.artifacts,
            &interpreter,
            &interpreter_path,
            InterpreterPlacement {
                bias: expected_bias,
                first_mapping: address,
                first_file: address,
            },
        )?;
    }
    Ok(())
}

pub fn main_reserved_range() -> TestResult {
    let mut case = Case::new(
        "main-reserved-range",
        Fixture::EntryNonPie,
        Naming::Absolute,
        None,
    )?;
    let mut main = check_io(fs::read(Fixture::EntryNonPie.path()))?;
    install_main(&mut case, &main)?;
    let pair = run_pair(&case, Mutation::None)?;
    if pair.artifacts.prepared.layout.load_bias != 0
        || pair.artifacts.prepared.layout.program_base != MIN_PROGRAM_ADDRESS
    {
        return Err("main boundary control did not start its ET_EXEC loads at 0x400000".into());
    }
    compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
    compare::observers(&pair).map_err(|failure| failure.to_string())?;

    // Mutate only the already-pinned target. The qualifying loader image and
    // metadata remain identical, so the defensive check must precede mapping
    // and the stale shadow geometry cannot stand in for the intended refusal.
    shift_main(&mut main, -4096);
    install_main(&mut case, &main)?;
    let address = MIN_PROGRAM_ADDRESS - 4096;
    match prepare_start_with_limits(
        &case.target,
        &case.invocation,
        Path::new("./h"),
        case.limits,
    ) {
        Err(Error::LowLoadSegment {
            address: observed,
            interpreter: false,
        }) if observed == address => {}
        result => {
            return Err(format!(
                "main PT_LOAD {address:#x} gave wrong preparation refusal: {result:?}"
            ));
        }
    }
    let native = trace::run(&case, None)?;
    native_observer_record(&native)?;
    let first_file = native_load_mappings(&native, &main, 0, &case.root.join("target"))?;
    if first_file != Some(address)
        || aux(&native.entry.aux, 3)? != address + word(&main, 32)
        || aux(&native.entry.aux, 9)? != word(&main, 24)
        || aux(&native.entry.aux, 7)? == 0
    {
        return Err(
            "low main native control did not independently witness its ELF mappings/auxv".into(),
        );
    }
    save_native_geometry_evidence(&case, &native, "main-3ff000")?;
    let start = Instant::now();
    let loaded = untraced_start(&case, Some(&pair.artifacts), "main-3ff000-loader")?;
    if loaded.status.code() != Some(127)
        || !loaded.stdout.is_empty()
        || loaded.stderr
            != b"reverie-elf-loader: program PT_LOAD overlaps loader reserved range -22\n"
    {
        return Err(format!(
            "reserved main PT_LOAD {address:#x} escaped the qualified loader guard: {loaded:?}"
        ));
    }
    println!(
        "main ET_EXEC: 0x400000 full parity PASS; 0x3ff000 native-valid exact maps/auxv/observer, host LowLoadSegment(false), loader exit127 exact diagnostic; native={:.6}s loader={:.6}s",
        native.elapsed.as_secs_f64(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

pub fn duplicate_header_mapping() -> TestResult {
    for fixture in [Fixture::EntryPie, Fixture::EntryNonPie] {
        let mut case = Case::new("last-header-mapping", fixture, Naming::Absolute, None)?;
        let mut main = check_io(fs::read(fixture.path()))?;
        let phoff = word(&main, 32);
        // The observer never runs the main's dynamic linker. Its unused
        // RELRO header can independently map the first file page a second
        // time, after every ordinary PT_LOAD, without duplicating GNU_STACK.
        let offset = header_offsets(&main)
            .into_iter()
            .find(|offset| kind(&main, *offset) == 0x6474e552)
            .expect("entry fixture has a replaceable GNU_RELRO");
        main[offset..offset + 56].fill(0);
        main[offset..offset + 4].copy_from_slice(&1u32.to_le_bytes());
        main[offset + 4..offset + 8].copy_from_slice(&4u32.to_le_bytes());
        put_word(&mut main, offset + 16, SECOND_HEADER_PAGE);
        put_word(&mut main, offset + 24, SECOND_HEADER_PAGE);
        put_word(&mut main, offset + 32, 4096);
        put_word(&mut main, offset + 40, 4096);
        put_word(&mut main, offset + 48, 4096);
        install_main(&mut case, &main)?;
        let pair = run_pair(&case, Mutation::None)?;
        let expected = pair.artifacts.prepared.layout.load_bias + SECOND_HEADER_PAGE + phoff;
        if aux(&pair.native.entry.aux, 3)? != expected
            || pair.artifacts.prepared.layout.phdr != expected
        {
            return Err(format!(
                "last header mapping did not supply AT_PHDR={expected:#x}"
            ));
        }
        compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
        compare::observers(&pair).map_err(|failure| failure.to_string())?;
        println!(
            "duplicate header page {}: native/loader AT_PHDR={expected:#x}",
            fixture.label()
        );
    }
    Ok(())
}

pub fn interpreter_load_span() -> TestResult {
    for (elf_type, label) in [(2u16, "et-exec"), (3, "et-dyn")] {
        let mut case = Case::new(
            &format!("interpreter-load-span-{label}"),
            Fixture::EntryNonPie,
            Naming::Absolute,
            None,
        )?;
        let mut main = check_io(fs::read(Fixture::EntryNonPie.path()))?;
        let entry = word(&main, 24);
        let text = header_offsets(&main)
            .into_iter()
            .find(|offset| {
                kind(&main, *offset) == 1
                    && word(&main, *offset + 16) <= entry
                    && entry < word(&main, *offset + 16) + word(&main, *offset + 32)
            })
            .expect("entry fixture maps its entry");
        let code = (word(&main, text + 8) + entry - word(&main, text + 16)) as usize;
        // Make reaching the already mapped main entry a successful exit.
        // The zero-span interpreter below must never transfer to this code.
        main[code..code + 11].copy_from_slice(&[
            0xb8, 0xe7, 0, 0, 0, // mov $SYS_exit_group, %eax
            0x31, 0xff, // xor %edi, %edi
            0x0f, 0x05, // syscall
            0x0f, 0x0b, // ud2
        ]);
        let interpreter_path = case.root.join("span-interpreter");
        set_interpreter(&mut main, &interpreter_path);
        install_main(&mut case, &main)?;
        let mut interpreter = check_io(fs::read(env!("ELF_LOADER_OBSERVER")))?;
        interpreter.truncate(64);
        interpreter.resize(120, 0);
        interpreter[16..18].copy_from_slice(&elf_type.to_le_bytes());
        interpreter[56..58].copy_from_slice(&1u16.to_le_bytes());
        interpreter[58..64].fill(0); // no section table
        put_word(&mut interpreter, 24, entry);
        put_word(&mut interpreter, 32, 64);
        put_word(&mut interpreter, 40, 0);
        interpreter[64..68].copy_from_slice(&1u32.to_le_bytes());
        interpreter[68..72].copy_from_slice(&5u32.to_le_bytes());
        put_word(&mut interpreter, 80, INTERPRETER_HINT);
        put_word(&mut interpreter, 88, INTERPRETER_HINT);
        put_word(&mut interpreter, 104, 1); // positive span: one byte of BSS
        put_word(&mut interpreter, 112, 4096);
        write_fixture(
            &interpreter_path,
            &interpreter,
            "one-byte interpreter load span; entry in main",
        )?;
        let pair = run_pair(&case, Mutation::None)?;
        compare::entries_with_zero_interpreter_bias(&case, &pair)
            .map_err(|failure| failure.to_string())?;
        if !pair.native.stdout.is_empty() || !pair.loaded.stdout.is_empty() {
            return Err("positive-span control unexpectedly wrote output".into());
        }

        // Deliberately invalidate the pinned path after a qualifying prepare
        // to test the loader's own guard as well as the host refusal. The main,
        // prepared image, metadata and entry address are unchanged.
        put_word(&mut interpreter, 104, 0);
        write_fixture(
            &interpreter_path,
            &interpreter,
            "zero interpreter load span; entry still in main",
        )?;
        match prepare_start_with_limits(
            &case.target,
            &case.invocation,
            Path::new("./h"),
            case.limits,
        ) {
            Err(Error::ZeroInterpreterLoadSpan) => {}
            result => {
                return Err(format!(
                    "zero-span {label} preparation gave wrong refusal: {result:?}"
                ));
            }
        }
        let native = untraced_start(&case, None, "zero-span-native")?;
        if native.status.signal() != Some(libc::SIGSEGV)
            || !native.stdout.is_empty()
            || !native.stderr.is_empty()
        {
            return Err(format!(
                "zero-span {label} native control did not die silently with SIGSEGV: {native:?}"
            ));
        }
        let loaded = untraced_start(&case, Some(&pair.artifacts), "zero-span-loader")?;
        if loaded.status.code() != Some(127)
            || !loaded.stdout.is_empty()
            || loaded.stderr != b"reverie-elf-loader: interpreter has zero load span -22\n"
        {
            return Err(format!(
                "zero-span {label} escaped loader defensive refusal: {loaded:?}"
            ));
        }
        println!(
            "{label} interpreter: span1 native/loader parity PASS; span0 native SIGSEGV, host ZeroInterpreterLoadSpan, loader exit127 with exact diagnostic"
        );
    }
    Ok(())
}

fn install_interpreter_control_main(
    case: &mut Case,
    fixture: Fixture,
    interpreter_path: &Path,
) -> TestResult {
    let mut main = check_io(fs::read(fixture.path()))?;
    if matches!(fixture, Fixture::EntryNonPie) {
        let first = header_offsets(&main)
            .into_iter()
            .find(|offset| kind(&main, *offset) == 1)
            .expect("entry fixture has PT_LOAD");
        let relocation = HIGH_MAIN_ADDRESS - (word(&main, first + 16) & !4095);
        shift_main(&mut main, relocation as i64);
    }
    set_interpreter(&mut main, interpreter_path);
    install_main(case, &main)
}

fn shift_main(image: &mut [u8], relocation: i64) {
    let entry = word(image, 24).wrapping_add_signed(relocation);
    put_word(image, 24, entry);
    for offset in header_offsets(image) {
        for field in [16, 24] {
            let address = word(image, offset + field);
            if address != 0 {
                put_word(
                    image,
                    offset + field,
                    address.wrapping_add_signed(relocation),
                );
            }
        }
    }
}

fn mixed_empty_interpreter_image(first_nonempty: u64, empty_address: u64) -> TestResult<Vec<u8>> {
    let mut image = check_io(fs::read(env!("ELF_LOADER_OBSERVER")))?;
    let loads: Vec<_> = header_offsets(&image)
        .into_iter()
        .filter(|offset| kind(&image, *offset) == 1)
        .collect();
    if u16::from_le_bytes(image[16..18].try_into().unwrap()) != 3
        || loads.len() != 4
        || word(&image, loads[0] + 16) != 0
        || word(&image, loads[1] + 16) != 4096
        || word(&image, loads[1] + 32) == 0
        || word(&image, loads[2] + 32) != 0
        || word(&image, loads[2] + 40) != 0
    {
        return Err(
            "mixed empty controls require the observer's ordinary four-load geometry".into(),
        );
    }
    let relocation = first_nonempty - 4096;
    let entry = word(&image, 24) + relocation;
    put_word(&mut image, 24, entry);
    for offset in &loads[1..] {
        for field in [16, 24] {
            let address = word(&image, offset + field) + relocation;
            put_word(&mut image, offset + field, address);
        }
    }
    // elf_load does not call mmap or vm_brk_flags for this PT_LOAD. Even an
    // unaligned empty address establishes the bias for every later fixed load.
    put_word(&mut image, loads[0] + 8, empty_address & 4095);
    put_word(&mut image, loads[0] + 16, empty_address);
    put_word(&mut image, loads[0] + 24, empty_address);
    put_word(&mut image, loads[0] + 32, 0);
    put_word(&mut image, loads[0] + 40, 0);
    Ok(image)
}

fn empty_bss_interpreter_image(address: u64, elf_type: u16) -> TestResult<Vec<u8>> {
    let mut image = mixed_empty_interpreter_image(INTERPRETER_HINT, 0)?;
    let loads: Vec<_> = header_offsets(&image)
        .into_iter()
        .filter(|offset| kind(&image, *offset) == 1)
        .collect();
    image[16..18].copy_from_slice(&elf_type.to_le_bytes());
    let bss = if elf_type == 3 {
        // Replace the unused empty third load with the unchanged text header,
        // leaving room for BSS in the second header while preserving ascending
        // addresses. The truly empty first load fixes interpreter bias at zero.
        let text = image[loads[1]..loads[1] + 56].to_vec();
        image[loads[2]..loads[2] + 56].copy_from_slice(&text);
        loads[1]
    } else {
        // Fixed ET_EXEC also covers a first load with filesz=0, memsz>0.
        // ET_DYN would try this first BSS at zero under a PIE main and fail
        // natively, so it would not qualify that guard with both main kinds.
        loads[0]
    };
    image[bss..bss + 56].fill(0);
    image[bss..bss + 4].copy_from_slice(&1u32.to_le_bytes());
    image[bss + 4..bss + 8].copy_from_slice(&6u32.to_le_bytes());
    put_word(&mut image, bss + 16, address);
    put_word(&mut image, bss + 24, address);
    put_word(&mut image, bss + 40, 4096);
    put_word(&mut image, bss + 48, 4096);
    Ok(image)
}

fn qualify_zero_bias_interpreter(
    case: &Case,
    fixture: Fixture,
    pair: &Pair,
    interpreter: &[u8],
    interpreter_path: &Path,
    first_mapping: u64,
    first_file: u64,
) -> TestResult {
    qualify_effective_interpreter(
        case,
        fixture,
        pair,
        interpreter,
        interpreter_path,
        InterpreterPlacement {
            bias: 0,
            first_mapping,
            first_file,
        },
    )
}

fn qualify_effective_interpreter(
    case: &Case,
    fixture: Fixture,
    pair: &Pair,
    interpreter: &[u8],
    interpreter_path: &Path,
    placement: InterpreterPlacement,
) -> TestResult {
    native_effective_interpreter(case, &pair.native, interpreter, interpreter_path, placement)?;
    if matches!(fixture, Fixture::EntryNonPie) {
        if pair.artifacts.prepared.layout.load_bias != 0
            || pair.artifacts.prepared.layout.program_base != HIGH_MAIN_ADDRESS
        {
            return Err("empty-load control did not relocate its ET_EXEC main to 0x1000000".into());
        }
        compare::entries_with_zero_interpreter_bias(case, pair)
            .map_err(|failure| failure.to_string())?;
    } else if placement.bias == 0 {
        compare::entries_with_zero_interpreter_bias_for_pie(case, pair)
            .map_err(|failure| failure.to_string())?;
    } else {
        compare::entries_with_known_interpreter_bias_for_pie(case, pair, placement.bias)
            .map_err(|failure| failure.to_string())?;
    }
    compare::observers(pair).map_err(|failure| failure.to_string())?;
    let loaded_maps = native_map_rows(&pair.loaded.entry.maps)?;
    let image_name = pair
        .artifacts
        .image
        .to_str()
        .ok_or("qualifying loader pathname is not UTF-8")?;
    if !loaded_maps
        .iter()
        .any(|row| row.start == 0x100000 && row.permissions == "r-xp" && row.name == image_name)
    {
        return Err(
            "qualifying loader does not independently witness its own RX mapping at 0x100000"
                .into(),
        );
    }
    check_io(fs::write(
        case.root.join("qualified-loader.maps"),
        &pair.loaded.entry.maps,
    ))
}

fn refuse_zero_bias_interpreter(
    case: &Case,
    artifacts: &Artifacts,
    interpreter: &[u8],
    interpreter_path: &Path,
    address: u64,
    first_file: u64,
) -> TestResult {
    refuse_effective_interpreter(
        case,
        artifacts,
        interpreter,
        interpreter_path,
        InterpreterPlacement {
            bias: 0,
            first_mapping: address,
            first_file,
        },
    )
}

fn refuse_effective_interpreter(
    case: &Case,
    artifacts: &Artifacts,
    interpreter: &[u8],
    interpreter_path: &Path,
    placement: InterpreterPlacement,
) -> TestResult {
    let address = placement.first_mapping;
    match prepare_start_with_limits(
        &case.target,
        &case.invocation,
        Path::new("./h"),
        case.limits,
    ) {
        Err(Error::LowLoadSegment {
            address: observed,
            interpreter: true,
        }) if observed == address => {}
        result => {
            return Err(format!(
                "effective interpreter PT_LOAD {address:#x} gave wrong preparation refusal: {result:?}"
            ));
        }
    }
    let native = trace::run(case, None)?;
    native_effective_interpreter(case, &native, interpreter, interpreter_path, placement)?;
    let start = Instant::now();
    let loaded = untraced_start(
        case,
        Some(artifacts),
        &format!("effective-{address:x}-loader"),
    )?;
    let loader_elapsed = start.elapsed();
    if loaded.status.code() != Some(127)
        || !loaded.stdout.is_empty()
        || loaded.stderr
            != b"reverie-elf-loader: interpreter PT_LOAD overlaps loader reserved range -22\n"
    {
        return Err(format!(
            "effective interpreter PT_LOAD {address:#x} escaped the qualified loader guard: {loaded:?}"
        ));
    }
    check_io(fs::write(
        case.root
            .join(format!("effective-{address:x}-guard.witness")),
        format!(
            "PASS address={address:#x} AT_BASE={:#x} host=LowLoadSegment(interpreter=true) native_status=0 loader_status=127 exact_stderr=true native_elapsed={:.6}s loader_elapsed={:.6}s\n",
            placement.bias,
            native.elapsed.as_secs_f64(),
            loader_elapsed.as_secs_f64()
        ),
    ))?;
    println!(
        "effective interpreter {address:#x}: native-valid AT_BASE={:#x}, exact RIP/maps/observer; host LowLoadSegment(true); qualified loader exit127 exact diagnostic; native={:.6}s loader={:.6}s",
        placement.bias,
        native.elapsed.as_secs_f64(),
        loader_elapsed.as_secs_f64()
    );
    Ok(())
}

fn native_effective_interpreter(
    case: &Case,
    native: &trace::Run,
    interpreter: &[u8],
    interpreter_path: &Path,
    placement: InterpreterPlacement,
) -> TestResult {
    native_observer_record(native)?;
    let entry = word(interpreter, 24).wrapping_add(placement.bias);
    let first_file = native_load_mappings(native, interpreter, placement.bias, interpreter_path)?;
    let rows = native_map_rows(&native.entry.maps)?;
    let first_mapping = rows
        .iter()
        .filter(|row| row.start < HIGH_MAIN_ADDRESS)
        .map(|row| row.start)
        .min();
    if aux(&native.entry.aux, 7)? != placement.bias
        || aux(&native.entry.proc_aux, 7)? != placement.bias
        || native.entry.registers.rip != entry
        || first_mapping != Some(placement.first_mapping)
        || first_file != Some(placement.first_file)
    {
        return Err(format!(
            "native empty/BSS interpreter did not witness effective placement: expected AT_BASE={:#x}, first mapping={:#x}, first file={:#x}, RIP={entry:#x}; actual AT_BASE={:#x}, first mapping={first_mapping:#x?}, first file={first_file:#x?}, RIP={:#x}",
            placement.bias,
            placement.first_mapping,
            placement.first_file,
            aux(&native.entry.aux, 7)?,
            native.entry.registers.rip
        ));
    }
    save_native_geometry_evidence(
        case,
        native,
        &format!(
            "effective-{:x}-base-{:x}",
            placement.first_mapping, placement.bias
        ),
    )?;
    println!(
        "native effective interpreter: first mapping={:#x}, first file={:#x}, AT_BASE={:#x}, RIP={entry:#x}, exact file/BSS mappings and assembly stack witness, {:.6}s",
        placement.first_mapping,
        placement.first_file,
        placement.bias,
        native.elapsed.as_secs_f64()
    );
    Ok(())
}

fn native_observer_record(native: &trace::Run) -> TestResult {
    let record = &native.stdout;
    if native.exit != 0
        || !native.stderr.is_empty()
        || record.len() < 2368
        || &record[..8] != b"ELFOBS01"
        || word(record, 8) != 2368
        || word(record, 184) != native.entry.stack.len() as u64
        || record.len() != 2368 + native.entry.stack.len()
        || word(record, 72) != native.entry.registers.gpr[7]
        || word(record, 176) != native.entry.stack_start
        || record[2368..] != native.entry.stack
    {
        return Err(format!(
            "native geometry control did not independently observe a successful complete entry: status={} record_len={} stack_len={} stderr={:?}",
            native.exit,
            record.len(),
            native.entry.stack.len(),
            native.stderr
        ));
    }
    for (index, value) in native.entry.registers.gpr.iter().enumerate() {
        if word(record, 16 + index * 8) != *value {
            return Err(format!(
                "native geometry observer's GPR{index} differs from ptrace"
            ));
        }
    }
    Ok(())
}

struct NativeMap<'a> {
    start: u64,
    end: u64,
    permissions: &'a str,
    offset: u64,
    device: &'a str,
    inode: u64,
    name: &'a str,
}

fn native_map_rows(maps: &[u8]) -> TestResult<Vec<NativeMap<'_>>> {
    std::str::from_utf8(maps)
        .map_err(|error| error.to_string())?
        .lines()
        .map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 5 {
                return Err(format!("geometry maps row lacks fields: {line}"));
            }
            let (start, end) = fields[0]
                .split_once('-')
                .ok_or("geometry maps range is malformed")?;
            Ok(NativeMap {
                start: u64::from_str_radix(start, 16).map_err(|error| error.to_string())?,
                end: u64::from_str_radix(end, 16).map_err(|error| error.to_string())?,
                permissions: fields[1],
                offset: u64::from_str_radix(fields[2], 16).map_err(|error| error.to_string())?,
                device: fields[3],
                inode: fields[4]
                    .parse::<u64>()
                    .map_err(|error| error.to_string())?,
                name: fields.get(5).copied().unwrap_or(""),
            })
        })
        .collect()
}

fn native_load_mappings(
    native: &trace::Run,
    image: &[u8],
    bias: u64,
    path: &Path,
) -> TestResult<Option<u64>> {
    let rows = native_map_rows(&native.entry.maps)?;
    let name = path
        .to_str()
        .ok_or("native geometry pathname is not UTF-8")?;
    let mut file_count = 0;
    for offset in header_offsets(image) {
        if kind(image, offset) != 1 {
            continue;
        }
        let address = word(image, offset + 16).wrapping_add(bias);
        let filesz = word(image, offset + 32);
        let memsz = word(image, offset + 40);
        if filesz > 0 {
            file_count += 1;
            let start = address & !4095;
            let end = (address + filesz + 4095) & !4095;
            let flags = kind(image, offset + 4);
            let permissions = format!(
                "{}{}{}p",
                if flags & 4 != 0 { 'r' } else { '-' },
                if flags & 2 != 0 { 'w' } else { '-' },
                if flags & 1 != 0 { 'x' } else { '-' }
            );
            let file_offset = word(image, offset + 8) - (address & 4095);
            if !rows.iter().any(|row| {
                row.start == start
                    && row.end == end
                    && row.permissions == permissions
                    && row.offset == file_offset
                    && row.name == name
                    && row.inode != 0
                    && row.device != "00:00"
            }) {
                return Err(format!(
                    "native file PT_LOAD mapping was not exact: {start:#x}..{end:#x} {permissions} offset={file_offset:#x} path={name}"
                ));
            }
        }
        if memsz > filesz {
            let start = if filesz == 0 {
                address & !4095
            } else {
                (address + filesz + 4095) & !4095
            };
            let end = (address + memsz + 4095) & !4095;
            let permissions = if kind(image, offset + 4) & 1 == 0 {
                "rw-p"
            } else {
                "rwxp"
            };
            if start < end
                && !rows.iter().any(|row| {
                    row.start == start
                        && row.end == end
                        && row.permissions == permissions
                        && row.offset == 0
                        && row.device == "00:00"
                        && row.inode == 0
                        && row.name.is_empty()
                })
            {
                return Err(format!(
                    "native anonymous BSS PT_LOAD mapping was not exact: {start:#x}..{end:#x} {permissions}"
                ));
            }
        }
    }
    let file_rows: Vec<_> = rows.iter().filter(|row| row.name == name).collect();
    if file_rows.len() != file_count {
        return Err(format!(
            "native geometry mapped {} file rows, expected {file_count}",
            file_rows.len()
        ));
    }
    Ok(file_rows.iter().map(|row| row.start).min())
}

fn save_native_geometry_evidence(case: &Case, native: &trace::Run, label: &str) -> TestResult {
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
        case.root.join(format!("{label}-native.witness")),
        format!(
            "PASS status=0 AT_PHDR={:#x} AT_BASE={:#x} AT_ENTRY={:#x} RIP={:#x} exact_PT_LOAD_maps=true independent_assembly_entry=true elapsed={:.6}s\n",
            aux(&native.entry.aux, 3)?,
            aux(&native.entry.aux, 7)?,
            aux(&native.entry.aux, 9)?,
            native.entry.registers.rip,
            native.elapsed.as_secs_f64()
        ),
    ))
}

fn interpreter_hint_image(hint: u64) -> TestResult<Vec<u8>> {
    let mut image = check_io(fs::read(env!("ELF_LOADER_OBSERVER")))?;
    let first = header_offsets(&image)
        .into_iter()
        .find(|offset| kind(&image, *offset) == 1)
        .expect("observer has PT_LOAD");
    if u16::from_le_bytes(image[16..18].try_into().unwrap()) != 3
        || word(&image, first + 16) & !4095 != 0
    {
        return Err("interpreter hint controls require the zero-based ET_DYN observer".into());
    }
    let entry = word(&image, 24) + hint;
    put_word(&mut image, 24, entry);
    for offset in header_offsets(&image) {
        if kind(&image, offset) == 1 {
            let address = word(&image, offset + 16) + hint;
            put_word(&mut image, offset + 16, address);
            put_word(&mut image, offset + 24, address);
        }
    }
    Ok(image)
}

fn interpreter_mapping_span(image: &[u8]) -> u64 {
    let loads: Vec<_> = header_offsets(image)
        .into_iter()
        .filter(|offset| kind(image, *offset) == 1)
        .collect();
    let low = loads
        .iter()
        .map(|offset| word(image, offset + 16) & !4095)
        .min()
        .unwrap();
    let high = loads
        .iter()
        .map(|offset| word(image, offset + 16) + word(image, offset + 40))
        .max()
        .unwrap();
    high - low
}

fn native_free_interpreter_hint(
    case: &Case,
    native: &trace::Run,
    interpreter: &[u8],
    interpreter_path: &Path,
) -> TestResult {
    let first = header_offsets(interpreter)
        .into_iter()
        .find(|offset| kind(interpreter, *offset) == 1)
        .expect("observer has PT_LOAD");
    let hint = word(interpreter, first + 16) & !4095;
    let span = interpreter_mapping_span(interpreter);
    let span_end = hint + ((span + 4095) & !4095);
    let entry = word(interpreter, 24);
    if hint == 0
        || span_end > HIGH_MAIN_ADDRESS
        || native.exit != 0
        || !native.stderr.is_empty()
        || aux(&native.entry.aux, 7)? != 0
        || aux(&native.entry.proc_aux, 7)? != 0
        || native.entry.registers.rip != entry
    {
        return Err(format!(
            "native interpreter hint {hint:#x} did not qualify a free hinted span: span={span:#x}, status={}, RIP={:#x}",
            native.exit, native.entry.registers.rip
        ));
    }
    let name = interpreter_path
        .to_str()
        .ok_or("geometry interpreter pathname is not UTF-8")?;
    let maps = std::str::from_utf8(&native.entry.maps).map_err(|error| error.to_string())?;
    let mut first_file_address = None;
    let mut entry_mapping = false;
    let mut span_end_mapping = false;
    for line in maps.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 5 {
            return Err("native maps row lacks required fields".into());
        }
        let (start, end) = fields[0]
            .split_once('-')
            .ok_or("native maps range is malformed")?;
        let start = u64::from_str_radix(start, 16).map_err(|error| error.to_string())?;
        let end = u64::from_str_radix(end, 16).map_err(|error| error.to_string())?;
        let mapping_name = fields.get(5).copied().unwrap_or("");
        if mapping_name == name {
            first_file_address =
                Some(first_file_address.map_or(start, |previous: u64| previous.min(start)));
            if start < hint || end > span_end {
                return Err(format!(
                    "native interpreter mapping escaped its hinted span: {line}"
                ));
            }
            entry_mapping |= start <= entry && entry < end && fields[1] == "r-xp";
        } else if start < span_end && hint < end && !mapping_name.is_empty() {
            return Err(format!("native hinted span overlaps another image: {line}"));
        }
        span_end_mapping |= start < span_end && span_end <= end;
    }
    if first_file_address != Some(hint) || !entry_mapping || !span_end_mapping {
        return Err(format!(
            "native maps did not independently witness interpreter hint {hint:#x}, executable entry {entry:#x}, and complete span ending {span_end:#x}"
        ));
    }
    for (suffix, bytes) in [
        ("maps", &native.entry.maps),
        ("stdout", &native.stdout),
        ("stderr", &native.stderr),
    ] {
        check_io(fs::write(
            case.root.join(format!("hint-{hint:x}-native.{suffix}")),
            bytes,
        ))?;
    }
    check_io(fs::write(
        case.root.join(format!("hint-{hint:x}-native.witness")),
        format!(
            "PASS hint={hint:#x} span={span:#x} mapped_end={span_end:#x} main_base={HIGH_MAIN_ADDRESS:#x} AT_BASE=0 RIP={entry:#x} native_elapsed={:.6}s\n",
            native.elapsed.as_secs_f64()
        ),
    ))?;
    println!(
        "native hinted interpreter {hint:#x}: span={span:#x}, mapped end={span_end:#x}, AT_BASE=0, RIP={entry:#x}, {:.6}s",
        native.elapsed.as_secs_f64()
    );
    Ok(())
}

pub(super) fn install_main(case: &mut Case, image: &[u8]) -> TestResult {
    let path = case.root.join("target");
    write_fixture(&path, image, "entry fixture ELF header/code mutation")?;
    case.target = check_io(File::open(&path))?;
    case.invocation = error(Invocation::execve(path.as_os_str()))?;
    Ok(())
}

pub(super) fn set_interpreter(image: &mut Vec<u8>, path: &Path) {
    let offset = header_offsets(image)
        .into_iter()
        .find(|offset| kind(image, *offset) == 3)
        .expect("entry fixture has PT_INTERP");
    let start = image.len() as u64;
    let name = path.as_os_str().as_bytes();
    image.extend_from_slice(name);
    image.push(0);
    put_word(image, offset + 8, start);
    put_word(image, offset + 32, name.len() as u64 + 1);
    put_word(image, offset + 40, name.len() as u64 + 1);
}

pub(super) fn write_fixture(path: &Path, image: &[u8], operation: &str) -> TestResult {
    check_io(fs::write(path, image))?;
    check_io(fs::set_permissions(path, Permissions::from_mode(0o755)))?;
    let hash = check_io(Command::new("sha256sum").arg(path).output())?;
    if !hash.status.success() {
        return Err("sha256sum failed for ELF geometry fixture".into());
    }
    let mut evidence = check_io(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(output_root()?.join("geometry-images.provenance")),
    )?;
    check_io(writeln!(
        evidence,
        "operation: {operation}\nsha256: {}",
        String::from_utf8_lossy(&hash.stdout).trim_end()
    ))
}

pub(super) fn untraced_start(
    case: &Case,
    artifacts: Option<&Artifacts>,
    label: &str,
) -> TestResult<Output> {
    let path = artifacts.map_or(case.invocation.path(), |value| {
        value.prepared.padded_path.as_c_str()
    });
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
            .expect("fixture environment has '='");
        command.env(
            OsStr::from_bytes(&bytes[..separator]),
            OsStr::from_bytes(&bytes[separator + 1..]),
        );
    }
    let target_fd = case.target.as_raw_fd();
    let metadata_fd = artifacts.map(|value| value.metadata.as_raw_fd());
    let data_limit = case.limits.data;
    // SAFETY: prepared child values are copied integers. After fork this
    // callback allocates nothing, takes no locks and issues only syscalls.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(target_fd, TARGET_FD) < 0
                || libc::fcntl(
                    TARGET_FD,
                    libc::F_SETFD,
                    if metadata_fd.is_some() {
                        0
                    } else {
                        libc::FD_CLOEXEC
                    },
                ) < 0
            {
                return Err(io::Error::last_os_error());
            }
            if let Some(fd) = metadata_fd
                && (libc::dup2(fd, METADATA_FD) < 0
                    || libc::fcntl(METADATA_FD, libc::F_SETFD, 0) < 0)
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
            let core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            let data = libc::rlimit {
                rlim_cur: data_limit,
                rlim_max: data_limit,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &core) < 0
                || libc::setrlimit(libc::RLIMIT_DATA, &data) < 0
            {
                return Err(io::Error::last_os_error());
            }
            libc::alarm(30);
            Ok(())
        });
    }
    let output = check_io(command.output())?;
    check_io(fs::write(
        case.root.join(format!("{label}.stdout")),
        &output.stdout,
    ))?;
    check_io(fs::write(
        case.root.join(format!("{label}.stderr")),
        &output.stderr,
    ))?;
    Ok(output)
}

fn aux(entries: &[(u64, u64)], requested: u64) -> TestResult<u64> {
    entries
        .iter()
        .find_map(|(kind, value)| (*kind == requested).then_some(*value))
        .ok_or_else(|| format!("auxv type {requested} missing"))
}

pub(super) fn header_offsets(image: &[u8]) -> Vec<usize> {
    assert_eq!(u16::from_le_bytes(image[54..56].try_into().unwrap()), 56);
    let count = u16::from_le_bytes(image[56..58].try_into().unwrap()) as usize;
    let start = word(image, 32) as usize;
    (0..count).map(|index| start + index * 56).collect()
}

pub(super) fn kind(image: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(image[offset..offset + 4].try_into().unwrap())
}

pub(super) fn word(image: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(image[offset..offset + 8].try_into().unwrap())
}

fn put_word(image: &mut [u8], offset: usize, value: u64) {
    image[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
