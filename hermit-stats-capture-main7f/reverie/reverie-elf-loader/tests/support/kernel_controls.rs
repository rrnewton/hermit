/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;

use reverie_elf_loader::Error;
use reverie_elf_loader::prepare_start_with_limits;

use super::Artifacts;
use super::Case;
use super::Fixture;
use super::Mutation;
use super::Naming;
use super::TestResult;
use super::compare;
use super::geometry;
use super::io as check_io;
use super::run_pair;
use super::trace;

const PAGE: u64 = 4096;
const STACK_TOP: u64 = 0x7ffffffff000;
const STACK_LOW: u64 = STACK_TOP - 8 * 1024 * 1024;
const FIXED_INTERPRETER: u64 = 0x800000;
const MAPPING_LIMIT: u64 = 16 * 1024 * 1024 * 1024;

pub fn kernel_mapping_size_controls() -> TestResult {
    for interpreter in [false, true] {
        let label = if interpreter { "interpreter" } else { "main" };
        let mut case = Case::new(
            &format!("kernel-mapping-size-{label}"),
            Fixture::EntryNonPie,
            Naming::Absolute,
            None,
        )?;
        let mut image = if interpreter {
            // Consume the first reservation without mmap so the boundary is
            // exactly this individual load's extent, rather than its span.
            fixed_interpreter(true)?
        } else {
            check_io(fs::read(Fixture::EntryNonPie.path()))?
        };
        let data = *load_offsets(&image)
            .last()
            .ok_or("mapping-size fixture lacks its final data load")?;
        if kind(&image, data + 4) != 6 || word(&image, data + 32) == 0 {
            return Err("mapping-size fixture needs a file-backed RW data load".into());
        }
        let address = word(&image, data + 16);
        let page = address & !(PAGE - 1);
        let memsize = MAPPING_LIMIT - (address - page);
        let zero_start = (address + word(&image, data + 32) + PAGE - 1) & !(PAGE - 1);
        put_word(&mut image, data + 40, memsize);
        let interpreter_path = case.root.join("mapping-interpreter");
        install_high_image(&mut case, &image, &interpreter_path, interpreter)?;
        let pair = run_pair(&case, Mutation::None)?;
        qualify_pair(&case, &pair, interpreter)?;
        require_anon_mapping(&pair.native.entry.maps, zero_start, page + MAPPING_LIMIT)?;
        require_anon_mapping(&pair.loaded.entry.maps, zero_start, page + MAPPING_LIMIT)?;

        // This only adds lazy anonymous virtual pages. The small original
        // data file and observer access pattern are retained without creating
        // a huge sparse file or touching the additional BSS pages.
        put_word(&mut image, data + 40, memsize + PAGE);
        install_high_image(&mut case, &image, &interpreter_path, interpreter)?;
        expect_refusal(
            &case,
            Refusal::Mapping {
                address: page,
                size: MAPPING_LIMIT + PAGE,
                interpreter,
            },
        )?;
        let native = trace::run(&case, None)?;
        compare::observer_witness(&native, "native mapping-size refusal control")
            .map_err(|failure| failure.to_string())?;
        let end = page + MAPPING_LIMIT + PAGE;
        require_anon_mapping(&native.entry.maps, zero_start, end)?;
        if !interpreter && native.entry.stat.get(&47) != Some(&end) {
            return Err("native mapping-size control did not establish its exact brk".into());
        }
        save_native(&case, &native, "mapping-size")?;
        defensive_refusal(
            &case,
            &pair.artifacts,
            "mapping exceeds allocator size bound",
            "mapping-size-defensive",
        )?;
        println!(
            "{label} mapping-size scope: native 16GiB+PAGE load has exact BSS/observer witness; named preparation/defensive refusal; 16GiB companion has exact parity"
        );
    }
    mapping_reservation_control()
}

fn mapping_reservation_control() -> TestResult {
    let mut case = Case::new(
        "kernel-mapping-reservation",
        Fixture::EntryNonPie,
        Naming::Absolute,
        None,
    )?;
    let mut image = fixed_interpreter(false)?;
    let empty = header_offsets(&image)
        .into_iter()
        .find(|offset| kind(&image, *offset) == 0x6474e552)
        .ok_or("reservation fixture needs replaceable GNU_RELRO header")?;
    image[empty..empty + 56].fill(0);
    put_kind(&mut image, empty, 1);
    put_kind(&mut image, empty + 4, 6);
    let address = FIXED_INTERPRETER + MAPPING_LIMIT;
    put_word(&mut image, empty + 16, address);
    put_word(&mut image, empty + 24, address);
    put_word(&mut image, empty + 48, PAGE);
    let interpreter_path = case.root.join("reservation-interpreter");
    install_high_image(&mut case, &image, &interpreter_path, true)?;
    let pair = run_pair(&case, Mutation::None)?;
    qualify_pair(&case, &pair, true)?;
    require_unmapped_page(&pair.native.entry.maps, address)?;
    require_unmapped_page(&pair.loaded.entry.maps, address)?;

    // The interpreter's empty high header grows only its initial read-only
    // reservation. Actual mapped bytes and main shadow metadata stay fixed.
    // No anonymous pages or large sparse file are needed for this boundary.
    put_word(&mut image, empty + 16, address + PAGE);
    put_word(&mut image, empty + 24, address + PAGE);
    install_high_image(&mut case, &image, &interpreter_path, true)?;
    expect_refusal(
        &case,
        Refusal::Mapping {
            address: FIXED_INTERPRETER,
            size: MAPPING_LIMIT + PAGE,
            interpreter: true,
        },
    )?;
    let native = trace::run(&case, None)?;
    compare::observer_witness(&native, "native first-reservation refusal control")
        .map_err(|failure| failure.to_string())?;
    require_unmapped_page(&native.entry.maps, address + PAGE)?;
    if native.entry.stat.get(&47) != Some(&pair.artifacts.prepared.layout.start_brk) {
        return Err("interpreter reservation control changed the main brk".into());
    }
    save_native(&case, &native, "mapping-reservation")?;
    defensive_refusal(
        &case,
        &pair.artifacts,
        "mapping exceeds allocator size bound",
        "mapping-reservation-defensive",
    )?;
    println!(
        "interpreter reservation scope: native 16GiB+PAGE read-only reservation reaches observer and releases empty high page; named preparation/defensive refusal; 16GiB companion has exact parity"
    );
    Ok(())
}

pub fn kernel_bss_right_merge_controls() -> TestResult {
    for interpreter in [false, true] {
        let label = if interpreter { "interpreter" } else { "main" };
        let mut case = Case::new(
            &format!("kernel-bss-right-merge-{label}"),
            Fixture::EntryNonPie,
            Naming::Absolute,
            None,
        )?;
        let source = if interpreter {
            env!("ELF_LOADER_OBSERVER")
        } else {
            Fixture::EntryNonPie.path()
        };
        let mut image = right_merge_image(source, interpreter)?;
        let interpreter_path = case.root.join("merge-interpreter");
        if interpreter {
            geometry::write_fixture(
                &interpreter_path,
                &image,
                "fixed observer with ascending overlapping loads; no right anonymous BSS",
            )?;
            let mut main = check_io(fs::read(Fixture::EntryNonPie.path()))?;
            geometry::set_interpreter(&mut main, &interpreter_path);
            geometry::install_main(&mut case, &main)?;
        } else {
            geometry::install_main(&mut case, &image)?;
        }
        let pair = run_pair(&case, Mutation::None)?;
        qualify_pair(&case, &pair, interpreter)?;

        // Only this earlier load grows. The later code/data continue to set
        // start_data, end_data and brk, so the qualified image and metadata
        // remain valid witnesses of the freestanding defensive guard.
        let loads = load_offsets(&image);
        put_word(&mut image, loads[0] + 40, 0x6000);
        if interpreter {
            geometry::write_fixture(
                &interpreter_path,
                &image,
                "earlier R load now creates a surviving right anonymous BSS",
            )?;
        } else {
            geometry::install_main(&mut case, &image)?;
        }
        let base = if interpreter {
            FIXED_INTERPRETER
        } else {
            0x400000
        };
        expect_refusal(
            &case,
            Refusal::RightMerge {
                end: base + 0x3000,
                interpreter,
            },
        )?;
        let native = trace::run(&case, None)?;
        compare::observer_witness(&native, "native right-merge refusal control")
            .map_err(|failure| failure.to_string())?;
        require_anon_mapping(&native.entry.maps, base + 0x1000, base + 0x3000)?;
        require_anon_mapping(&native.entry.maps, base + 0x3000, base + 0x6000)?;
        save_native(&case, &native, "right-merge")?;
        defensive_refusal(
            &case,
            &pair.artifacts,
            "anonymous BSS would merge with right VMA",
            "right-merge-defensive",
        )?;
        println!(
            "{label} BSS: native retains [{:#x},{:#x}) and [{:#x},{:#x}); named preparation/defensive refusal; no-right-neighbor companion has exact parity",
            base + 0x1000,
            base + 0x3000,
            base + 0x3000,
            base + 0x6000,
        );
    }
    cross_image_bss_right_merge_control()
}

pub fn kernel_initial_stack_overlap_controls() -> TestResult {
    for interpreter in [false, true] {
        let label = if interpreter { "interpreter" } else { "main" };
        let mut case = Case::new(
            &format!("kernel-stack-overlap-{label}"),
            Fixture::EntryNonPie,
            Naming::Absolute,
            None,
        )?;
        let (mut image, extra_load) = high_load_image(interpreter, STACK_LOW - PAGE)?;
        let interpreter_path = case.root.join("stack-interpreter");
        install_high_image(&mut case, &image, &interpreter_path, interpreter)?;
        let pair = run_pair(&case, Mutation::None)?;
        qualify_pair(&case, &pair, interpreter)?;
        require_anon_mapping(&pair.native.entry.maps, STACK_LOW - PAGE, STACK_LOW)?;
        require_anon_mapping(&pair.loaded.entry.maps, STACK_LOW - PAGE, STACK_LOW)?;

        let address = STACK_TOP - PAGE;
        put_word(&mut image, extra_load + 16, address);
        put_word(&mut image, extra_load + 24, address);
        install_high_image(&mut case, &image, &interpreter_path, interpreter)?;
        expect_refusal(
            &case,
            Refusal::Stack {
                address,
                interpreter,
            },
        )?;
        // Native creates the ELF tables after replacing this stack page, and
        // therefore still reaches the observer with its newly built auxv.
        // Argument and execfn strings overwritten by the segment are retained
        // exactly in this independent ptrace/assembly witness.
        let native = trace::run(&case, None)?;
        compare::observer_witness(&native, "native stack-overlap refusal control")
            .map_err(|failure| failure.to_string())?;
        require_anon_mapping_named(&native.entry.maps, address, STACK_TOP, "[stack]")?;
        if !native.entry.execfn().is_empty() {
            return Err("native stack overlap did not overwrite its execfn bytes".into());
        }
        save_native(&case, &native, "stack-overlap")?;
        defensive_refusal(
            &case,
            &pair.artifacts,
            "PT_LOAD overlaps initial stack",
            "stack-defensive",
        )?;
        println!(
            "{label} stack overwrite: native exact maps/register/assembly stack witness; named preparation/defensive refusal; adjacent lower-band companion has exact parity"
        );
    }
    shadow_stack_overlap_control()
}

pub fn kernel_reserved_top_page_controls() -> TestResult {
    let top_page_is_native_valid = native_top_page_probe()?;
    for interpreter in [false, true] {
        let label = if interpreter { "interpreter" } else { "main" };
        let mut case = Case::new(
            &format!("kernel-reserved-top-page-{label}"),
            Fixture::EntryNonPie,
            Naming::Absolute,
            None,
        )?;
        let (mut image, extra_load) = high_load_image(interpreter, STACK_LOW - PAGE)?;
        let interpreter_path = case.root.join("top-interpreter");
        install_high_image(&mut case, &image, &interpreter_path, interpreter)?;
        let pair = run_pair(&case, Mutation::None)?;
        qualify_pair(&case, &pair, interpreter)?;

        put_word(&mut image, extra_load + 16, STACK_TOP);
        put_word(&mut image, extra_load + 24, STACK_TOP);
        install_high_image(&mut case, &image, &interpreter_path, interpreter)?;
        expect_refusal(
            &case,
            Refusal::TopPage {
                address: STACK_TOP,
                interpreter,
            },
        )?;
        if top_page_is_native_valid {
            let native = trace::run(&case, None)?;
            compare::observer_witness(&native, "native LA57 top-page refusal control")
                .map_err(|failure| failure.to_string())?;
            require_anon_mapping(&native.entry.maps, STACK_TOP, STACK_TOP + PAGE)?;
            if native.entry.execfn() != case.invocation.native_execfn().as_bytes() {
                return Err("native LA57 top-page control disturbed the initial stack".into());
            }
            save_native(&case, &native, "top-page")?;
        } else {
            let native = geometry::untraced_start(&case, None, "native-4level-top-page")?;
            require_native_sigsegv(&native)?;
        }
        defensive_refusal(
            &case,
            &pair.artifacts,
            "PT_LOAD reaches reserved top page",
            "top-page-defensive",
        )?;
        println!(
            "{label} top page: native LA57 support={top_page_is_native_valid}; exact named preparation/defensive refusal; lower-band companion has exact parity"
        );
    }
    Ok(())
}

pub fn kernel_interpreter_entry_controls() -> TestResult {
    let mut case = Case::new(
        "kernel-interpreter-entry",
        Fixture::EntryNonPie,
        Naming::Absolute,
        None,
    )?;
    let interpreter_path = case.root.join("entry-interpreter");
    let mut interpreter = fixed_interpreter(false)?;
    geometry::write_fixture(
        &interpreter_path,
        &interpreter,
        "fixed interpreter with an admitted executable entry",
    )?;
    let mut main = check_io(fs::read(Fixture::EntryNonPie.path()))?;
    geometry::set_interpreter(&mut main, &interpreter_path);
    geometry::install_main(&mut case, &main)?;
    let pair = run_pair(&case, Mutation::None)?;
    qualify_pair(&case, &pair, true)?;

    for address in [0x3fffff, 0x100000, STACK_TOP, 0x100000000000000] {
        put_word(&mut interpreter, 24, address);
        geometry::write_fixture(
            &interpreter_path,
            &interpreter,
            &format!("interpreter entry now outside admitted range: {address:#x}"),
        )?;
        expect_refusal(&case, Refusal::Entry { address })?;
        let native = geometry::untraced_start(&case, None, &format!("native-entry-{address:x}"))?;
        require_native_sigsegv(&native)?;
        defensive_refusal(
            &case,
            &pair.artifacts,
            "interpreter entry outside admitted address range",
            &format!("entry-{address:x}-defensive"),
        )?;
    }
    println!(
        "interpreter entry: native low/hole/TASK_SIZE controls SIGSEGV; exact named preparation/defensive refusals; executable fixed-entry companion has exact parity"
    );
    Ok(())
}

fn right_merge_image(source: &str, interpreter: bool) -> TestResult<Vec<u8>> {
    let mut image = check_io(fs::read(source))?;
    let loads = load_offsets(&image);
    if loads.len() != 4 || image.len() < 0x3000 {
        return Err("right-merge fixture needs four loads and at least three file pages".into());
    }
    let text = image[loads[1]..loads[1] + 56].to_vec();
    let base = if interpreter {
        FIXED_INTERPRETER
    } else {
        0x400000
    };
    let relocation = if interpreter {
        FIXED_INTERPRETER + 0xf000
    } else {
        0xf000
    };
    let entry = word(&image, 24) + relocation;
    put_word(&mut image, 24, entry);
    put_word(&mut image, loads[0] + 16, base);
    put_word(&mut image, loads[0] + 24, base);
    put_word(&mut image, loads[0] + 32, 0x3000);
    put_word(&mut image, loads[0] + 40, 0x3000);
    image[loads[1]..loads[1] + 56].fill(0);
    put_kind(&mut image, loads[1], 1);
    put_kind(&mut image, loads[1] + 4, 6);
    put_word(&mut image, loads[1] + 16, base + 0x1000);
    put_word(&mut image, loads[1] + 24, base + 0x1000);
    put_word(&mut image, loads[1] + 40, 0x2000);
    put_word(&mut image, loads[1] + 48, PAGE);
    image[loads[2]..loads[2] + 56].copy_from_slice(&text);
    put_word(&mut image, loads[2] + 16, base + 0x10000);
    put_word(&mut image, loads[2] + 24, base + 0x10000);
    for field in [16, 24] {
        let address = word(&image, loads[3] + field) + relocation;
        put_word(&mut image, loads[3] + field, address);
    }
    if interpreter {
        image[16..18].copy_from_slice(&2_u16.to_le_bytes());
    }
    Ok(image)
}

fn fixed_interpreter(empty_first: bool) -> TestResult<Vec<u8>> {
    let mut image = check_io(fs::read(env!("ELF_LOADER_OBSERVER")))?;
    let entry = word(&image, 24) + FIXED_INTERPRETER;
    put_word(&mut image, 24, entry);
    image[16..18].copy_from_slice(&2_u16.to_le_bytes());
    for offset in load_offsets(&image) {
        for field in [16, 24] {
            let address = word(&image, offset + field) + FIXED_INTERPRETER;
            put_word(&mut image, offset + field, address);
        }
    }
    if empty_first {
        let first = load_offsets(&image)[0];
        put_word(&mut image, first + 32, 0);
        put_word(&mut image, first + 40, 0);
    }
    Ok(image)
}

fn high_load_image(interpreter: bool, address: u64) -> TestResult<(Vec<u8>, usize)> {
    let mut image = if interpreter {
        // Consume total_size with an empty first load, so the final high load
        // is tested independently of the whole-image first reservation.
        fixed_interpreter(true)?
    } else {
        check_io(fs::read(Fixture::EntryNonPie.path()))?
    };
    let extra = header_offsets(&image)
        .into_iter()
        .find(|offset| kind(&image, *offset) == 0x6474e552)
        .ok_or("high-load control needs replaceable GNU_RELRO header")?;
    image[extra..extra + 56].fill(0);
    put_kind(&mut image, extra, 1);
    put_kind(&mut image, extra + 4, 6);
    put_word(&mut image, extra + 16, address);
    put_word(&mut image, extra + 24, address);
    put_word(&mut image, extra + 40, PAGE);
    put_word(&mut image, extra + 48, PAGE);
    Ok((image, extra))
}

fn shadow_stack_overlap_control() -> TestResult {
    for (label, address) in [
        ("below", STACK_LOW - PAGE + 0x123),
        ("inside", STACK_LOW + 0x123),
    ] {
        let mut case = Case::new(
            &format!("kernel-shadow-empty-{label}"),
            Fixture::EntryNonPie,
            Naming::Absolute,
            None,
        )?;
        let (mut image, empty) = high_load_image(false, address)?;
        put_word(&mut image, empty + 8, 0x123);
        put_word(&mut image, empty + 40, 0);
        geometry::install_main(&mut case, &image)?;
        let pair = run_pair(&case, Mutation::None)?;
        qualify_pair(&case, &pair, false)?;
        let emitted = check_io(fs::read(&pair.artifacts.image))?;
        let shadow = load_offsets(&emitted)
            .into_iter()
            .find(|offset| word(&emitted, offset + 16) == address)
            .ok_or("generated image lacks the empty unaligned shadow header")?;
        if word(&emitted, shadow + 32) != 0 || word(&emitted, shadow + 40) != 0 {
            return Err("empty native load acquired padding-only shadow BSS".into());
        }
        let page = address & !(PAGE - 1);
        for (field, expected) in [(45, address), (46, address), (47, page + PAGE)] {
            if pair.native.entry.stat.get(&field) != Some(&expected) {
                return Err(format!(
                    "native empty load did not establish stat{field}={expected:#x}: {:?}",
                    pair.native.entry.stat
                ));
            }
        }
        let maps =
            std::str::from_utf8(&pair.native.entry.maps).map_err(|error| error.to_string())?;
        for line in maps.lines() {
            let range = line
                .split_whitespace()
                .next()
                .ok_or("empty native maps row")?;
            let (start, end) = range.split_once('-').ok_or("invalid native map range")?;
            let start = u64::from_str_radix(start, 16).map_err(|error| error.to_string())?;
            let end = u64::from_str_radix(end, 16).map_err(|error| error.to_string())?;
            if start < page + PAGE && end > page {
                return Err(format!(
                    "empty native load unexpectedly mapped its page: {line}"
                ));
            }
        }
        if pair.native.peak_data_kib != pair.loaded.peak_data_kib {
            return Err(format!(
                "empty shadow changed peak VmData: native={}KiB loader={}KiB",
                pair.native.peak_data_kib, pair.loaded.peak_data_kib
            ));
        }
        save_native(&case, &pair.native, "empty-shadow")?;
    }
    println!(
        "unaligned empty final loads below/inside stack band: exact native/loader parity, no mapped page, shadow filesz=memsz=0, exact data/brk metadata and VmData peak"
    );
    Ok(())
}

fn cross_image_bss_right_merge_control() -> TestResult {
    let mut case = Case::new(
        "kernel-bss-right-merge-cross-image",
        Fixture::EntryNonPie,
        Naming::Absolute,
        None,
    )?;
    let interpreter_path = case.root.join("cross-merge-interpreter");
    let mut interpreter = right_merge_image(env!("ELF_LOADER_OBSERVER"), true)?;
    let first = load_offsets(&interpreter)[0];
    put_word(&mut interpreter, first + 32, 0);
    put_word(&mut interpreter, first + 40, 0);
    geometry::write_fixture(
        &interpreter_path,
        &interpreter,
        "empty-first fixed observer; left BSS ends at main right BSS",
    )?;
    let (mut main, right) = high_load_image(false, FIXED_INTERPRETER + 0x3000)?;
    put_kind(&mut main, right + 4, 7); // Different X bit prevents a native or mmap merge.
    put_word(&mut main, right + 40, 0x3000);
    geometry::set_interpreter(&mut main, &interpreter_path);
    geometry::install_main(&mut case, &main)?;
    let pair = run_pair(&case, Mutation::None)?;
    qualify_pair(&case, &pair, true)?;
    require_mapping(
        &pair.native.entry.maps,
        FIXED_INTERPRETER + 0x3000,
        FIXED_INTERPRETER + 0x6000,
        "rwxp",
        "",
    )?;

    // Preserve every address, size, data field and emitted shadow byte. Only
    // the right VMA's X bit changes, making the interpreter's RW BSS mergeable.
    put_kind(&mut main, right + 4, 6);
    geometry::install_main(&mut case, &main)?;
    expect_refusal(
        &case,
        Refusal::RightMerge {
            end: FIXED_INTERPRETER + 0x3000,
            interpreter: true,
        },
    )?;
    let native = trace::run(&case, None)?;
    compare::observer_witness(&native, "native cross-image BSS refusal control")
        .map_err(|failure| failure.to_string())?;
    require_anon_mapping(
        &native.entry.maps,
        FIXED_INTERPRETER + 0x1000,
        FIXED_INTERPRETER + 0x3000,
    )?;
    require_anon_mapping(
        &native.entry.maps,
        FIXED_INTERPRETER + 0x3000,
        FIXED_INTERPRETER + 0x6000,
    )?;
    save_native(&case, &native, "cross-image-right-merge")?;
    defensive_refusal(
        &case,
        &pair.artifacts,
        "anonymous BSS would merge with right VMA",
        "cross-image-right-merge-defensive",
    )?;
    println!(
        "cross-image BSS: native retains interpreter/main right-adjacent RW VMAs; exact preparation/defensive refusal; same-geometry RWX-right companion has exact parity"
    );
    Ok(())
}

fn install_high_image(
    case: &mut Case,
    image: &[u8],
    interpreter_path: &Path,
    interpreter: bool,
) -> TestResult {
    if interpreter {
        geometry::write_fixture(interpreter_path, image, "fixed high-address BSS control")?;
        let mut main = check_io(fs::read(Fixture::EntryNonPie.path()))?;
        geometry::set_interpreter(&mut main, interpreter_path);
        geometry::install_main(case, &main)
    } else {
        geometry::install_main(case, image)
    }
}

fn qualify_pair(case: &Case, pair: &super::Pair, zero_interpreter_bias: bool) -> TestResult {
    if zero_interpreter_bias {
        compare::entries_with_zero_interpreter_bias(case, pair)
            .map_err(|failure| failure.to_string())?;
    } else {
        compare::entries(case, pair).map_err(|failure| failure.to_string())?;
    }
    compare::observers(pair).map_err(|failure| failure.to_string())
}

#[derive(Debug)]
enum Refusal {
    RightMerge {
        end: u64,
        interpreter: bool,
    },
    Stack {
        address: u64,
        interpreter: bool,
    },
    TopPage {
        address: u64,
        interpreter: bool,
    },
    Entry {
        address: u64,
    },
    Mapping {
        address: u64,
        size: u64,
        interpreter: bool,
    },
}

fn expect_refusal(case: &Case, expected: Refusal) -> TestResult {
    let result = prepare_start_with_limits(
        &case.target,
        &case.invocation,
        &case.root.join("h"),
        case.limits,
    );
    let matches = match (&expected, &result) {
        (
            Refusal::Mapping {
                address,
                size,
                interpreter,
            },
            Err(Error::MappingTooLarge {
                address: actual_address,
                size: actual_size,
                interpreter: actual_interpreter,
            }),
        ) => address == actual_address && size == actual_size && interpreter == actual_interpreter,
        (
            Refusal::RightMerge { end, interpreter },
            Err(Error::BssRightMerge {
                address,
                interpreter: actual_interpreter,
            }),
        ) => end == address && interpreter == actual_interpreter,
        (
            Refusal::Stack {
                address,
                interpreter,
            },
            Err(Error::InitialStackOverlap {
                address: actual_address,
                size,
                interpreter: actual_interpreter,
            }),
        ) => address == actual_address && *size == PAGE && interpreter == actual_interpreter,
        (
            Refusal::TopPage {
                address,
                interpreter,
            },
            Err(Error::ReservedTopPage {
                address: actual_address,
                interpreter: actual_interpreter,
            }),
        ) => address == actual_address && interpreter == actual_interpreter,
        (
            Refusal::Entry { address },
            Err(Error::InterpreterEntry {
                address: actual_address,
            }),
        ) => address == actual_address,
        _ => false,
    };
    if !matches {
        return Err(format!(
            "kernel fidelity refusal {expected:?} gave wrong preparation result: {result:?}"
        ));
    }
    Ok(())
}

fn defensive_refusal(
    case: &Case,
    artifacts: &Artifacts,
    diagnostic: &str,
    label: &str,
) -> TestResult {
    let output = geometry::untraced_start(case, Some(artifacts), label)?;
    let expected = format!("reverie-elf-loader: {diagnostic} -22\n");
    if output.status.code() != Some(127)
        || !output.stdout.is_empty()
        || output.stderr != expected.as_bytes()
    {
        return Err(format!(
            "defensive guard {diagnostic:?} did not refuse before handoff: {output:?}"
        ));
    }
    Ok(())
}

fn require_native_sigsegv(output: &std::process::Output) -> TestResult {
    use std::os::unix::process::ExitStatusExt;
    if output.status.signal() != Some(libc::SIGSEGV)
        || !output.stdout.is_empty()
        || !output.stderr.is_empty()
    {
        return Err(format!("native refused-entry control differs: {output:?}"));
    }
    Ok(())
}

fn require_anon_mapping(maps: &[u8], start: u64, end: u64) -> TestResult {
    require_anon_mapping_named(maps, start, end, "")
}

fn require_unmapped_page(maps: &[u8], page: u64) -> TestResult {
    let maps = std::str::from_utf8(maps).map_err(|error| error.to_string())?;
    for line in maps.lines() {
        let range = line.split_whitespace().next().ok_or("empty maps row")?;
        let (start, end) = range.split_once('-').ok_or("invalid map range")?;
        let start = u64::from_str_radix(start, 16).map_err(|error| error.to_string())?;
        let end = u64::from_str_radix(end, 16).map_err(|error| error.to_string())?;
        if start < page + PAGE && end > page {
            return Err(format!(
                "empty load unexpectedly retained a mapped page: {line}"
            ));
        }
    }
    Ok(())
}

fn require_anon_mapping_named(maps: &[u8], start: u64, end: u64, name: &str) -> TestResult {
    require_mapping(maps, start, end, "rw-p", name)
}

fn require_mapping(maps: &[u8], start: u64, end: u64, permissions: &str, name: &str) -> TestResult {
    let maps = std::str::from_utf8(maps).map_err(|error| error.to_string())?;
    let range = format!("{start:08x}-{end:08x}");
    if !maps.lines().any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        fields.len() == if name.is_empty() { 5 } else { 6 }
            && fields[0] == range
            && fields[1] == permissions
            && fields[2] == "00000000"
            && fields[3] == "00:00"
            && fields[4] == "0"
            && fields.get(5).copied().unwrap_or("") == name
    }) {
        return Err(format!(
            "native control lacks exact anonymous RW mapping {range}:\n{maps}"
        ));
    }
    Ok(())
}

fn save_native(case: &Case, native: &trace::Run, label: &str) -> TestResult {
    if native.exit != 0 || !native.stderr.is_empty() {
        return Err(format!(
            "native {label} did not exit cleanly: status={} stderr={:?}",
            native.exit, native.stderr
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
        case.root.join(format!("{label}-native.witness")),
        format!(
            "native_status=0 RIP={:#x} RSP={:#x} observer_bytes={} stack_bytes={} elapsed_seconds={:.6}\n",
            native.entry.registers.rip,
            native.entry.registers.gpr[7],
            native.stdout.len(),
            native.entry.stack.len(),
            native.elapsed.as_secs_f64(),
        ),
    ))
}

fn native_top_page_probe() -> TestResult<bool> {
    // SAFETY: the non-replacing probe maps one inaccessible anonymous page
    // and removes only that page on success; it never overwrites a live VMA.
    let result = unsafe {
        libc::mmap(
            STACK_TOP as *mut libc::c_void,
            PAGE as usize,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    if result == libc::MAP_FAILED {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOMEM) => Ok(false),
            Some(libc::EEXIST) => Ok(true),
            _ => Err(format!(
                "native TASK_SIZE probe failed unexpectedly: {error}"
            )),
        };
    }
    if result as u64 != STACK_TOP {
        // SAFETY: result is the page this successful mmap just allocated.
        unsafe { libc::munmap(result, PAGE as usize) };
        return Err("native TASK_SIZE probe ignored FIXED_NOREPLACE".into());
    }
    // SAFETY: remove exactly the page allocated above.
    if unsafe { libc::munmap(result, PAGE as usize) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(true)
}

fn header_offsets(image: &[u8]) -> Vec<usize> {
    let phoff = word(image, 32) as usize;
    let phnum = u16::from_le_bytes(image[56..58].try_into().expect("ELF field"));
    (0..usize::from(phnum))
        .map(|index| phoff + index * 56)
        .collect()
}

fn load_offsets(image: &[u8]) -> Vec<usize> {
    header_offsets(image)
        .into_iter()
        .filter(|offset| kind(image, *offset) == 1)
        .collect()
}

fn kind(image: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(image[offset..offset + 4].try_into().expect("ELF field"))
}

fn word(image: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(image[offset..offset + 8].try_into().expect("ELF field"))
}

fn put_kind(image: &mut [u8], offset: usize, value: u32) {
    image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_word(image: &mut [u8], offset: usize, value: u64) {
    image[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
