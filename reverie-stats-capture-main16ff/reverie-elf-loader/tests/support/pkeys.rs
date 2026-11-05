/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::arch::x86_64::__cpuid;
use std::arch::x86_64::__cpuid_count;
use std::arch::x86_64::_xgetbv;
use std::fs;

use super::Artifacts;
use super::Case;
use super::Fixture;
use super::Mutation;
use super::Naming;
use super::Pair;
use super::TestResult;
use super::compare;
use super::geometry::header_offsets;
use super::geometry::install_main;
use super::geometry::kind;
use super::geometry::set_interpreter;
use super::geometry::word;
use super::geometry::write_fixture;
use super::io as check_io;
use super::trace;

pub fn pkey_text_case(
    test: &str,
    main_execute_only: bool,
    interpreter_execute_only: bool,
    probe_interpreter: bool,
) -> TestResult {
    let features = __cpuid_count(7, 0);
    let pku = __cpuid(0).eax >= 7 && features.ecx & (1 << 3) != 0;
    let ospke = __cpuid(0).eax >= 7 && features.ecx & (1 << 4) != 0;
    if !pku || !ospke {
        println!("{test}: SKIP: CPU requires PKU and OSPKE (PKU={pku}, OSPKE={ospke})");
        return Ok(());
    }
    if __cpuid(1).ecx & (1 << 27) == 0 {
        return Err("OSPKE was advertised without OSXSAVE".into());
    }
    // SAFETY: OSXSAVE was checked, and zero selects XCR0.
    let xcr0 = unsafe { _xgetbv(0) };
    if xcr0 & (1 << 9) == 0 {
        return Err("OSPKE was advertised without PKRU in XCR0".into());
    }
    for fixture in [Fixture::EntryNonPie, Fixture::EntryPie] {
        let probe = if probe_interpreter {
            "interpreter"
        } else {
            "main"
        };
        let mut case = Case::new(&format!("{test}-{probe}"), fixture, Naming::Absolute, None)?;
        let mut main = check_io(fs::read(fixture.path()))?;
        let main_byte = text_flags(&mut main, main_execute_only)?;
        let source = if probe_interpreter {
            env!("ELF_LOADER_PKEY_PROBE_INTERPRETER")
        } else {
            env!("ELF_LOADER_PKEY_PROBE_MAIN")
        };
        let mut interpreter = check_io(fs::read(source))?;
        let interpreter_byte = text_flags(&mut interpreter, interpreter_execute_only)?;
        let interpreter_path = case.root.join("pkey-interpreter");
        write_fixture(
            &interpreter_path,
            &interpreter,
            &format!(
                "live PKRU/read probe; interpreter text execute-only={interpreter_execute_only}"
            ),
        )?;
        set_interpreter(&mut main, &interpreter_path);
        install_main(&mut case, &main)?;
        let artifacts = Artifacts::new(&case, Mutation::None, false)?;
        let seed = main_execute_only || interpreter_execute_only;
        let native = trace::run_pkey_control(&case, None, false)?;
        let loaded = trace::run_pkey_control(&case, Some(&artifacts), seed)?;
        let pair = Pair {
            native,
            loaded,
            artifacts,
        };
        let native_pkru = entry_pkru(&pair.native)?;
        let loaded_pkru = entry_pkru(&pair.loaded)?;
        println!(
            "{test} {} probe={probe}: native PKRU={native_pkru:#x} readable={}, loader PKRU={loaded_pkru:#x} readable={}",
            fixture.label(),
            pair.native.stdout.get(24).copied().unwrap_or(255),
            pair.loaded.stdout.get(24).copied().unwrap_or(255),
        );
        for (mode, run) in [("native", &pair.native), ("loader", &pair.loaded)] {
            let witness = run.pkey_witness.as_ref().ok_or("pkey witness missing")?;
            check_io(fs::write(
                case.root.join(format!("{mode}.smaps")),
                &witness.smaps,
            ))?;
            check_io(fs::write(
                case.root.join(format!("{mode}.pkey-witness")),
                format!(
                    "entry_PKRU={:#x} seed={:?} main_execute_only={main_execute_only} interpreter_execute_only={interpreter_execute_only} probe={probe}\n",
                    entry_pkru(run)?,
                    witness.seed,
                ),
            ))?;
        }
        let main_address = pair.artifacts.prepared.layout.entry;
        let interpreter_address = pair.native.entry.registers.rip;
        let (address, execute_only, byte) = if probe_interpreter {
            (
                interpreter_address,
                interpreter_execute_only,
                interpreter_byte,
            )
        } else {
            (main_address, main_execute_only, main_byte)
        };
        for (mode, run) in [("native", &pair.native), ("loader", &pair.loaded)] {
            let smaps = &run
                .pkey_witness
                .as_ref()
                .ok_or("pkey witness missing")?
                .smaps;
            mapping_key(smaps, main_address, main_execute_only, mode)?;
            mapping_key(smaps, interpreter_address, interpreter_execute_only, mode)?;
        }
        if pair.native.pkey_witness.as_ref().unwrap().seed.is_some() {
            return Err("native PKRU witness was seeded".into());
        }
        let loaded_seed = pair.loaded.pkey_witness.as_ref().unwrap().seed;
        if seed {
            let (before, after) = loaded_seed.ok_or("loader PKRU seed witness missing")?;
            if after != before & !0xc || native_pkru != after | 4 || after == native_pkru {
                return Err(format!(
                    "PKRU control was not discriminating: seed={before:#x}->{after:#x}, native={native_pkru:#x}"
                ));
            }
        } else if loaded_seed.is_some() {
            return Err("readable companion must preserve the ordinary unseeded PKRU state".into());
        }
        // Qualify native behavior independently, then retain every ordinary
        // entry, stack, maps and register comparison for the seeded loader.
        probe_record(&pair.native, address, execute_only, byte, "native")?;
        compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
        probe_record(&pair.loaded, address, execute_only, byte, "loader")?;
        if pair.native.stdout != pair.loaded.stdout {
            return Err("native/loader live PKRU and readability records differ".into());
        }
        println!(
            "{test} {}: exact entry/PKRU/maps parity; {probe} {}",
            fixture.label(),
            if execute_only {
                "SIGSEGV/SEGV_PKUERR pkey=1"
            } else {
                "read returned exact ELF byte"
            },
        );
    }
    Ok(())
}

fn text_flags(image: &mut [u8], execute_only: bool) -> TestResult<u8> {
    let entry = word(image, 24);
    let loads: Vec<_> = header_offsets(image)
        .into_iter()
        .filter(|offset| kind(image, *offset) == 1)
        .collect();
    let text = *loads
        .iter()
        .find(|offset| {
            word(image, **offset + 16) <= entry
                && entry < word(image, **offset + 16) + word(image, **offset + 32)
        })
        .ok_or("probe fixture entry has no file-backed PT_LOAD")?;
    let flags = u32::from_le_bytes(image[text + 4..text + 8].try_into().unwrap());
    if flags != 5 || word(image, text + 32) != word(image, text + 40) {
        return Err("probe text must start RX with no BSS".into());
    }
    let start = word(image, text + 16) & !4095;
    let end = (word(image, text + 16) + word(image, text + 40) + 4095) & !4095;
    if loads.iter().any(|offset| {
        *offset != text
            && (word(image, *offset + 16) & !4095) < end
            && start < ((word(image, *offset + 16) + word(image, *offset + 40) + 4095) & !4095)
    }) {
        return Err("probe text overlaps another PT_LOAD page".into());
    }
    let file_offset = word(image, text + 8) + entry - word(image, text + 16);
    let byte = *image
        .get(file_offset as usize)
        .ok_or("probe entry byte is absent")?;
    if execute_only {
        image[text + 4..text + 8].copy_from_slice(&1u32.to_le_bytes());
    }
    Ok(byte)
}

fn entry_pkru(run: &trace::Run) -> TestResult<u32> {
    let bytes = run
        .entry
        .registers
        .extended
        .get("pkru")
        .ok_or("entry PKRU missing")?;
    let bytes: [u8; 4] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| "entry PKRU size is not four bytes")?;
    Ok(u32::from_le_bytes(bytes))
}

fn mapping_key(smaps: &[u8], address: u64, execute_only: bool, mode: &str) -> TestResult {
    let text = std::str::from_utf8(smaps).map_err(|error| error.to_string())?;
    let mut selected = false;
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if let Some((start, end)) = fields.first().and_then(|range| range.split_once('-')) {
            let start = u64::from_str_radix(start, 16).map_err(|error| error.to_string())?;
            let end = u64::from_str_radix(end, 16).map_err(|error| error.to_string())?;
            selected = start <= address && address < end;
            if selected
                && fields.get(1).copied() != Some(if execute_only { "--xp" } else { "r-xp" })
            {
                return Err(format!("{mode} probe text permissions are wrong: {line}"));
            }
        } else if selected && let Some(key) = line.strip_prefix("ProtectionKey:") {
            let key = key
                .trim()
                .parse::<u32>()
                .map_err(|error| error.to_string())?;
            if key != u32::from(execute_only) {
                return Err(format!(
                    "{mode} text at {address:#x} uses unexpected pkey {key}"
                ));
            }
            return Ok(());
        }
    }
    Err(format!(
        "{mode} smaps lacks a protection key for probe text at {address:#x}"
    ))
}

fn probe_record(
    run: &trace::Run,
    address: u64,
    execute_only: bool,
    byte: u8,
    mode: &str,
) -> TestResult {
    let mut expected = [0u8; 64];
    expected[..8].copy_from_slice(b"ELFPKU01");
    let pkru = entry_pkru(run)?;
    expected[8..12].copy_from_slice(&pkru.to_le_bytes());
    expected[12..16].copy_from_slice(&pkru.to_le_bytes());
    expected[16..24].copy_from_slice(&address.to_le_bytes());
    if execute_only {
        expected[40..44].copy_from_slice(&11u32.to_le_bytes()); // SIGSEGV
        expected[44..48].copy_from_slice(&4u32.to_le_bytes()); // SEGV_PKUERR
        expected[48..52].copy_from_slice(&1u32.to_le_bytes()); // si_pkey
        expected[56..64].copy_from_slice(&address.to_le_bytes());
    } else {
        expected[24..32].copy_from_slice(&1u64.to_le_bytes());
        expected[32] = byte;
    }
    if run.exit != 0 || !run.stderr.is_empty() || run.stdout != expected {
        return Err(format!(
            "{mode} PKRU/readability witness differs: exit={} stderr={:?} record={:?} expected={expected:?}",
            run.exit, run.stderr, run.stdout,
        ));
    }
    Ok(())
}
