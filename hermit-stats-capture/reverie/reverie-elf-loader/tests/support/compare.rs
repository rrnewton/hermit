/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;

use super::Case;
use super::ImageIdentity;
use super::Pair;
use super::TestResult;
use super::trace::Entry;
use super::trace::Registers;
use super::trace::decode_aux;

#[derive(Debug)]
pub(super) struct Failure {
    pub assertion: &'static str,
    detail: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "{}: {}", self.assertion, self.detail)
    }
}

type Check<T = ()> = Result<T, Failure>;

fn failure(assertion: &'static str, detail: impl Into<String>) -> Failure {
    Failure {
        assertion,
        detail: detail.into(),
    }
}

fn require(condition: bool, assertion: &'static str, detail: impl Into<String>) -> Check {
    if condition {
        Ok(())
    } else {
        Err(failure(assertion, detail))
    }
}

fn same<T: PartialEq + fmt::Debug>(
    native: &T,
    loaded: &T,
    assertion: &'static str,
    field: &str,
) -> Check {
    require(
        native == loaded,
        assertion,
        format!("{field}: native={native:?}, loader={loaded:?}"),
    )
}

pub(super) fn entries(case: &Case, pair: &Pair) -> Check {
    entries_with_expected_auxv_differences(case, pair, &[3, 5, 7, 9, 33])
}

pub(super) fn entries_with_zero_interpreter_bias(case: &Case, pair: &Pair) -> Check {
    same(
        &pair.artifacts.prepared.layout.load_bias,
        &0,
        "native_layout",
        "main image load bias for the zero interpreter bias control",
    )?;
    same(
        &aux_value(&pair.native.entry, 7)?,
        &0,
        "native_layout",
        "native AT_BASE for the zero interpreter bias control",
    )?;
    let origin_base = pair
        .loaded
        .origin
        .proc_aux
        .iter()
        .find_map(|value| (value.0 == 7).then_some(value.1))
        .ok_or_else(|| failure("proc_auxv", "loader exec origin lacks AT_BASE"))?;
    same(
        &origin_base,
        &0,
        "proc_auxv",
        "loader kernel-origin AT_BASE for the zero interpreter bias control",
    )?;
    let origin_vdso = pair
        .loaded
        .origin
        .proc_aux
        .iter()
        .find_map(|value| (value.0 == 33).then_some(value.1))
        .ok_or_else(|| failure("proc_auxv", "loader exec origin lacks AT_SYSINFO_EHDR"))?;
    same(
        &aux_value(&pair.native.entry, 33)?,
        &origin_vdso,
        "native_layout",
        "native vDSO equals loader kernel-origin vDSO for the low interpreter hint control",
    )?;
    entries_with_expected_auxv_differences(case, pair, &[3, 5, 9])
}

pub(super) fn entries_with_zero_interpreter_bias_for_pie(case: &Case, pair: &Pair) -> Check {
    entries_with_known_interpreter_bias_for_pie(case, pair, 0)
}

pub(super) fn entries_with_known_interpreter_bias_for_pie(
    case: &Case,
    pair: &Pair,
    expected_bias: u64,
) -> Check {
    require(
        pair.artifacts.prepared.layout.load_bias != 0,
        "native_layout",
        "PIE empty-first interpreter control requires a biased main image",
    )?;
    same(
        &aux_value(&pair.native.entry, 7)?,
        &expected_bias,
        "native_layout",
        "native AT_BASE for the PIE empty-first interpreter control",
    )?;
    let origin_base = pair
        .loaded
        .origin
        .proc_aux
        .iter()
        .find_map(|value| (value.0 == 7).then_some(value.1))
        .ok_or_else(|| failure("proc_auxv", "loader exec origin lacks AT_BASE"))?;
    same(
        &origin_base,
        &0,
        "proc_auxv",
        "loader kernel-origin AT_BASE for the PIE empty-first interpreter control",
    )?;
    let origin_vdso = pair
        .loaded
        .origin
        .proc_aux
        .iter()
        .find_map(|value| (value.0 == 33).then_some(value.1))
        .ok_or_else(|| failure("proc_auxv", "loader exec origin lacks AT_SYSINFO_EHDR"))?;
    same(
        &aux_value(&pair.native.entry, 33)?,
        &origin_vdso,
        "native_layout",
        "native vDSO equals loader kernel-origin vDSO for the PIE empty-first control",
    )?;
    // Empty-first interpreters never allocate an initial span near mmap_base.
    // AT_BASE differs through procfs only for the independently required
    // nonzero bias. Every existing stack/map/register/observer check remains.
    let expected_differences: &[u64] = if expected_bias == 0 {
        &[3, 5, 9]
    } else {
        &[3, 5, 7, 9]
    };
    entries_with_expected_auxv_differences(case, pair, expected_differences)
}

fn entries_with_expected_auxv_differences(
    case: &Case,
    pair: &Pair,
    expected_auxv_differences: &[u64],
) -> Check {
    same(&pair.native.exit, &0, "successful_exit", "native status")?;
    same(&pair.loaded.exit, &0, "successful_exit", "loader status")?;
    byte_same(
        &pair.native.stderr,
        &pair.loaded.stderr,
        "output_parity",
        "stderr",
    )?;
    require(
        pair.native.stderr.is_empty(),
        "output_parity",
        "native fixture unexpectedly wrote stderr",
    )?;
    let native = &pair.native.entry;
    let loaded = &pair.loaded.entry;
    same(
        &loaded.proc_aux,
        &pair.loaded.origin.proc_aux,
        "proc_auxv",
        "loader proc auxv preserves exact kernel exec origin values",
    )?;
    for field in [26, 27] {
        same(
            &loaded.stat[&field],
            &pair.loaded.origin.stat[&field],
            "proc_stat",
            &format!("loader code field{field} preserves kernel exec origin"),
        )?;
    }
    for field in [45, 46, 47] {
        same(
            &native.stat[&field],
            &loaded.stat[&field],
            "heap_metadata",
            &format!("stat field{field}"),
        )?;
    }
    for (field, value) in [
        (45, pair.artifacts.prepared.layout.start_data),
        (46, pair.artifacts.prepared.layout.end_data),
        (47, pair.artifacts.prepared.layout.start_brk),
    ] {
        same(
            &native.stat[&field],
            &value,
            "native_layout",
            &format!("computed stat field{field}"),
        )?;
    }
    maps(&native.maps, &loaded.maps, pair)?;
    registers(&native.registers, &loaded.registers)?;
    same(
        &native.stack_start,
        &loaded.stack_start,
        "initial_stack",
        "stack start",
    )?;
    byte_same(
        &native.stack,
        &loaded.stack,
        "initial_stack",
        "entire entry stack image",
    )?;
    require(
        native.stack[..1024].iter().all(|byte| *byte == 0),
        "initial_stack",
        "native bytes below entry RSP were modified before the witness",
    )?;
    same(
        &native.aux,
        &loaded.aux,
        "stack_auxv",
        "every ordered stack auxv entry including AT_NULL",
    )?;
    for (kind, expected) in [
        (3, pair.artifacts.prepared.layout.phdr),
        (5, u64::from(pair.artifacts.prepared.layout.phnum)),
        (9, pair.artifacts.prepared.layout.entry),
    ] {
        same(
            &aux_value(native, kind)?,
            &expected,
            "native_layout",
            &format!("computed auxv type{kind}"),
        )?;
    }
    byte_same(
        native.execfn(),
        pair.artifacts.prepared.native_execfn.as_bytes(),
        "execfn",
        "native AT_EXECFN",
    )?;
    byte_same(
        loaded.execfn(),
        native.execfn(),
        "execfn",
        "AT_EXECFN bytes",
    )?;
    proc_auxv(
        &native.aux,
        &native.proc_aux,
        &loaded.proc_aux,
        expected_auxv_differences,
    )?;
    proc_stat(&native.stat, &loaded.stat)?;
    let native_exe = fs::read_link(format!("/proc/self/fd/{}", case.target.as_raw_fd()))
        .map_err(|error| failure("executable_identity", error.to_string()))?;
    byte_same(
        &native.exe,
        native_exe.as_os_str().as_bytes(),
        "executable_identity",
        "native /proc/self/exe",
    )?;
    byte_same(
        &loaded.exe,
        pair.artifacts.image.as_os_str().as_bytes(),
        "executable_identity",
        "loader /proc/self/exe",
    )?;
    require(
        native.exe != loaded.exe,
        "expected_difference_set",
        "/proc/self/exe did not differ",
    )?;
    let mut expected_comm = pair.artifacts.prepared.comm.as_bytes().to_vec();
    expected_comm.push(b'\n');
    byte_same(&native.comm, &expected_comm, "comm", "native comm")?;
    byte_same(&loaded.comm, &native.comm, "comm", "comm")?;
    let expected_cmdline = case
        .argv
        .iter()
        .flat_map(|arg| arg.as_bytes_with_nul())
        .copied()
        .collect::<Vec<_>>();
    byte_same(
        &native.cmdline,
        &expected_cmdline,
        "cmdline",
        "native cmdline",
    )?;
    byte_same(&loaded.cmdline, &native.cmdline, "cmdline", "cmdline")?;
    require(
        pair.loaded.peak_data_kib <= pair.native.peak_data_kib,
        "peak_data_accounting",
        format!(
            "syscall-by-syscall VmData peak native={}KiB loader={}KiB",
            pair.native.peak_data_kib, pair.loaded.peak_data_kib
        ),
    )?;
    Ok(())
}

fn aux_value(entry: &Entry, kind: u64) -> Check<u64> {
    entry
        .aux
        .iter()
        .find_map(|value| (value.0 == kind).then_some(value.1))
        .ok_or_else(|| failure("stack_auxv", format!("type{kind} is absent")))
}

fn registers(native: &Registers, loaded: &Registers) -> Check {
    for (index, (left, right)) in native.gpr.iter().zip(&loaded.gpr).enumerate() {
        same(left, right, "entry_registers", &format!("GPR{index}"))?;
        if index != 7 {
            same(
                left,
                &0,
                "native_entry_registers",
                &format!("native GPR{index}"),
            )?;
        }
    }
    for (field, left, right) in [
        ("RIP", native.rip, loaded.rip),
        ("RFLAGS", native.rflags, loaded.rflags),
        ("FS base", native.fs_base, loaded.fs_base),
        ("GS base", native.gs_base, loaded.gs_base),
        ("XCR0", native.xcr0, loaded.xcr0),
        ("feature mask", native.feature_mask, loaded.feature_mask),
    ] {
        same(&left, &right, "entry_registers", field)?;
    }
    same(
        &native.rflags,
        &0x202,
        "native_entry_registers",
        "native RFLAGS",
    )?;
    same(&native.mxcsr, &loaded.mxcsr, "entry_registers", "MXCSR")?;
    same(
        &native.x87_control,
        &loaded.x87_control,
        "entry_registers",
        "x87 control word",
    )?;
    same(
        &native.x87_status,
        &loaded.x87_status,
        "entry_registers",
        "x87 status word",
    )?;
    same(
        &native.x87_tag,
        &loaded.x87_tag,
        "entry_registers",
        "x87 abridged tag word",
    )?;
    for (index, (name, expected)) in [
        ("CS", 0x33),
        ("SS", 0x2b),
        ("DS", 0),
        ("ES", 0),
        ("FS", 0),
        ("GS", 0),
    ]
    .into_iter()
    .enumerate()
    {
        same(
            &native.selectors[index],
            &loaded.selectors[index],
            "entry_registers",
            &format!("{name} selector"),
        )?;
        same(
            &native.selectors[index],
            &expected,
            "native_entry_registers",
            &format!("native {name} selector"),
        )?;
    }
    for (name, value) in [("FS", native.fs_base), ("GS", native.gs_base)] {
        same(
            &value,
            &0,
            "native_entry_registers",
            &format!("native {name} base"),
        )?;
    }
    same(
        &native.mxcsr,
        &0x1f80,
        "native_entry_registers",
        "native MXCSR",
    )?;
    same(
        &native.x87_control,
        &0x37f,
        "native_entry_registers",
        "native x87 control word",
    )?;
    same(
        &native.x87_status,
        &0,
        "native_entry_registers",
        "native x87 status word",
    )?;
    same(
        &native.x87_tag,
        &0,
        "native_entry_registers",
        "native x87 abridged tag word (all registers empty)",
    )?;
    byte_same(
        &native.xmm,
        &loaded.xmm,
        "entry_registers",
        "all XMM registers",
    )?;
    require(
        native.xmm.iter().all(|byte| *byte == 0),
        "native_entry_registers",
        "native XMM registers were nonzero",
    )?;
    same(
        &native.extended.keys().collect::<Vec<_>>(),
        &loaded.extended.keys().collect::<Vec<_>>(),
        "entry_registers",
        "advertised extended register components",
    )?;
    for (name, bytes) in &native.extended {
        byte_same(bytes, &loaded.extended[name], "entry_registers", name)?;
        if *name != "pkru" {
            require(
                bytes.iter().all(|byte| *byte == 0),
                "native_entry_registers",
                format!("native {name} was nonzero"),
            )?;
        }
    }
    Ok(())
}

fn proc_auxv(
    stack: &[(u64, u64)],
    native: &[(u64, u64)],
    loaded: &[(u64, u64)],
    expected_differences: &[u64],
) -> Check {
    same(
        &stack,
        &native,
        "proc_auxv",
        "native proc auxv equals its stack auxv",
    )?;
    same(
        &native.iter().map(|pair| pair.0).collect::<Vec<_>>(),
        &loaded.iter().map(|pair| pair.0).collect::<Vec<_>>(),
        "proc_auxv",
        "all ordered auxv entry types",
    )?;
    let changed = native
        .iter()
        .zip(loaded)
        .filter_map(|(left, right)| (left != right).then_some(left.0))
        .collect::<BTreeSet<_>>();
    let expected = expected_differences
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    same(
        &changed,
        &expected,
        "expected_difference_set",
        "exact differing /proc/auxv types",
    )
}

fn proc_stat(native: &BTreeMap<u32, u64>, loaded: &BTreeMap<u32, u64>) -> Check {
    same(
        &native.keys().collect::<Vec<_>>(),
        &loaded.keys().collect::<Vec<_>>(),
        "proc_stat",
        "selected stat fields",
    )?;
    let changed = native
        .iter()
        .filter_map(|(field, value)| (loaded[field] != *value).then_some(*field))
        .collect::<BTreeSet<_>>();
    same(
        &changed,
        &[26, 27].into_iter().collect::<BTreeSet<_>>(),
        "expected_difference_set",
        "exact differing selected /proc/stat fields",
    )
}

#[derive(Debug, PartialEq, Eq)]
struct Map {
    start: u64,
    end: u64,
    permissions: String,
    offset: u64,
    device: (u64, u64),
    inode: u64,
    name: String,
    raw: String,
}

impl Map {
    fn fields_match(&self, other: &Self) -> bool {
        self.start == other.start
            && self.end == other.end
            && self.permissions == other.permissions
            && self.offset == other.offset
            && self.device == other.device
            && self.inode == other.inode
            && self.name == other.name
    }
}

fn parse_maps(bytes: &[u8]) -> Check<Vec<Map>> {
    let text =
        std::str::from_utf8(bytes).map_err(|error| failure("mapping_parity", error.to_string()))?;
    text.lines()
        .map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 5 {
                return Err(failure("mapping_parity", "maps row lacks required fields"));
            }
            let (start, end) = fields[0]
                .split_once('-')
                .ok_or_else(|| failure("mapping_parity", "maps address range malformed"))?;
            let (major, minor) = fields[3]
                .split_once(':')
                .ok_or_else(|| failure("mapping_parity", "maps device malformed"))?;
            let hex = |value| {
                u64::from_str_radix(value, 16)
                    .map_err(|error| failure("mapping_parity", error.to_string()))
            };
            let mut name = line;
            for _ in 0..5 {
                name = name.trim_start();
                name = name
                    .find(char::is_whitespace)
                    .map_or("", |end| &name[end..]);
            }
            Ok(Map {
                start: hex(start)?,
                end: hex(end)?,
                permissions: fields[1].to_string(),
                offset: hex(fields[2])?,
                device: (hex(major)?, hex(minor)?),
                inode: fields[4]
                    .parse()
                    .map_err(|error: std::num::ParseIntError| {
                        failure("mapping_parity", error.to_string())
                    })?,
                name: name.trim_start().to_string(),
                raw: line.to_string(),
            })
        })
        .collect()
}

pub(super) fn initial_record_inode(bytes: &[u8], length: u64) -> TestResult<u64> {
    let maps = parse_maps(bytes).map_err(|error| error.to_string())?;
    let mapping = maps
        .into_iter()
        .find(|mapping| mapping.start == 0x200000)
        .ok_or("initial loader scratch VMA is absent")?;
    if length < 4096
        || !length.is_multiple_of(4096)
        || mapping.end != 0x200000 + length
        || mapping.permissions != "rw-s"
        || mapping.offset != 0
        || mapping.device != (0, 1)
        || mapping.inode == 0
        || mapping.name != "/dev/zero (deleted)"
    {
        return Err(format!(
            "unexpected initial loader scratch VMA: {mapping:?}"
        ));
    }
    Ok(mapping.inode)
}

pub(super) fn image_identity(image: &std::path::Path) -> TestResult<ImageIdentity> {
    let file = fs::File::open(image).map_err(|error| error.to_string())?;
    // Overlay stat st_dev can name the overlay rather than the VMA's backing
    // superblock. Observe this exact file's kernel mapping identity directly.
    // SAFETY: this is a new read-only one-page mapping of the opened image.
    let address = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    if address == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error().to_string());
    }
    struct Mapping(*mut libc::c_void);
    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: this guard owns this mapping and its exact length.
            unsafe {
                libc::munmap(self.0, 4096);
            }
        }
    }
    let mapping = Mapping(address);
    let bytes = fs::read("/proc/self/maps").map_err(|error| error.to_string())?;
    let maps = parse_maps(&bytes).map_err(|error| error.to_string())?;
    let address = address as u64;
    let observed = maps
        .into_iter()
        .find(|row| row.start == address && row.end == address + 4096)
        .ok_or("parent image witness mapping is absent or merged")?;
    if observed.permissions != "r--p"
        || observed.offset != 0
        || observed.name != image.to_string_lossy()
        || observed.inode == 0
    {
        return Err(format!(
            "parent image witness mapping is wrong: {observed:?}"
        ));
    }
    drop(mapping); // The witness mapping is gone before either child is forked.
    Ok(ImageIdentity {
        device: observed.device,
        inode: observed.inode,
        name: observed.name,
    })
}

fn maps(native: &[u8], loaded: &[u8], pair: &Pair) -> Check {
    let native = parse_maps(native)?;
    let loaded = parse_maps(loaded)?;
    let identity = &pair.artifacts.identity;
    let bytes = &pair.artifacts.template;
    let phoff = word(bytes, 32)? as usize;
    let phnum = half(bytes, 56)? as usize;
    let mut expected = Vec::new();
    for index in 0..phnum {
        let header = bytes
            .get(phoff + index * 56..phoff + (index + 1) * 56)
            .ok_or_else(|| failure("mapping_parity", "template header is truncated"))?;
        if u32::from_le_bytes(header[..4].try_into().expect("four bytes")) != 1 {
            continue;
        }
        let flags = u32::from_le_bytes(header[4..8].try_into().expect("four bytes"));
        let address = word(header, 16)?;
        let size = word(header, 40)?;
        require(
            address == 0x100000 && flags == 5,
            "mapping_parity",
            "template must contain only the RX segment at 0x100000",
        )?;
        expected.push(Map {
            start: address & !4095,
            end: (address + size + 4095) & !4095,
            permissions: "r-xp".into(),
            offset: word(header, 8)? & !4095,
            device: identity.device,
            inode: identity.inode,
            name: identity.name.clone(),
            raw: String::new(),
        });
    }
    require(
        expected.len() == 1,
        "mapping_parity",
        "template must have exactly one RX load mapping",
    )?;
    let mut ordinary = Vec::new();
    let mut seen_file = vec![0; expected.len()];
    let mut seen_record = 0;
    for mapping in loaded {
        if let Some(index) = expected
            .iter()
            .position(|expected| expected.fields_match(&mapping))
        {
            seen_file[index] += 1;
        } else if mapping.start == 0x200000 && mapping.end == 0x201000 {
            // Bind the newly allocated shmem inode to its original mmap stop,
            // rather than accepting an arbitrary inode for this known extra.
            require(
                mapping.permissions == "r--s"
                    && mapping.offset == 0
                    && mapping.device == (0, 1)
                    && Some(mapping.inode) == pair.loaded.record_inode
                    && mapping.name == "/dev/zero (deleted)",
                "mapping_parity",
                format!("unexpected final loader record mapping: {mapping:?}"),
            )?;
            seen_record += 1;
        } else {
            ordinary.push(mapping);
        }
    }
    require(
        seen_file.iter().all(|count| *count == 1) && seen_record == 1,
        "mapping_parity",
        format!("loader extra rows: RX counts={seen_file:?}, final record count={seen_record}"),
    )?;
    for name in ["[vvar]", "[vvar_vclock]", "[vdso]"] {
        let left = native
            .iter()
            .filter(|mapping| mapping.name == name)
            .collect::<Vec<_>>();
        let right = ordinary
            .iter()
            .filter(|mapping| mapping.name == name)
            .collect::<Vec<_>>();
        same(
            &left,
            &right,
            "mapping_parity",
            &format!("special mapping {name}"),
        )?;
    }
    same(
        &native,
        &ordinary,
        "mapping_parity",
        "every ordinary mapping (including vdso, vvar, interpreter, libc, heap and stack)",
    )
}

pub(super) fn layouts(case: &Case, pair: &Pair, finite_data: bool) -> Check {
    let native = report(&pair.native.stdout)?;
    let loaded = report(&pair.loaded.stdout)?;
    same(
        &native.keys().collect::<Vec<_>>(),
        &loaded.keys().collect::<Vec<_>>(),
        "layout_output",
        "all reporter keys",
    )?;
    let changed = native
        .iter()
        .filter_map(|(key, value)| (loaded[key] != *value).then_some(key.as_str()))
        .collect::<BTreeSet<_>>();
    let expected = ["exe", "proc_auxv", "maps", "stat.26", "stat.27"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    same(
        &changed,
        &expected,
        "expected_difference_set",
        "exact differing reporter fields",
    )?;
    for field in [
        "entry.rsp",
        "entry.stack_start",
        "entry.stack_len",
        "entry.stack",
        "argc",
        "envc",
        "auxc",
        "execfn.ptr",
        "execfn.hex",
        "program.base",
        "interpreter.base",
        "libc.base",
        "heap.start",
        "heap.end",
        "heap.growth_pages",
        "heap.first_failed_request",
        "heap.first_failed_result",
        "data.baseline_kib",
        "data.peak_kib",
        "stat.28",
        "stat.45",
        "stat.46",
        "stat.47",
        "stat.48",
        "stat.49",
        "stat.50",
        "stat.51",
        "comm",
        "cmdline",
    ] {
        require(
            native.contains_key(field),
            "layout_output",
            format!("required reporter field {field} is absent"),
        )?;
    }
    same(
        &number(&native, "argc")?,
        &(case.argv.len() as u64),
        "argv",
        "argc",
    )?;
    same(
        &number(&native, "envc")?,
        &(case.env.len() as u64),
        "envp",
        "envc",
    )?;
    for (report, witness) in [(&native, &pair.native.entry), (&loaded, &pair.loaded.entry)] {
        same(
            &number(report, "entry.rsp")?,
            &witness.registers.gpr[7],
            "initial_stack",
            "reporter stack pointer equals original kernel stack pointer",
        )?;
        same(
            &number(report, "entry.stack_start")?,
            &witness.registers.gpr[7],
            "initial_stack",
            "reporter stack witness starts at original RSP",
        )?;
        let stack = unhex(field(report, "entry.stack")?)?;
        same(
            &number(report, "entry.stack_len")?,
            &(stack.len() as u64),
            "initial_stack",
            "reporter stack witness length",
        )?;
        byte_same(
            &stack,
            &witness.stack[1024..],
            "initial_stack",
            "reporter stack image equals independent pre-interpreter witness",
        )?;
    }
    for (prefix, values) in [("argv", &case.argv), ("env", &case.env)] {
        for (index, value) in values.iter().enumerate() {
            let bytes = unhex(field(&native, &format!("{prefix}.{index}.hex"))?)?;
            byte_same(
                &bytes,
                value.as_bytes(),
                "argv_envp",
                &format!("{prefix} entry{index}"),
            )?;
            require(
                number(&native, &format!("{prefix}.{index}.ptr"))? != 0,
                "argv_envp",
                "null string pointer",
            )?;
        }
    }
    same(
        &number(&native, "auxc")?,
        &(pair.native.entry.aux.len() as u64),
        "stack_auxv",
        "auxc",
    )?;
    for (index, &(kind, value)) in pair.native.entry.aux.iter().enumerate() {
        same(
            &number(&native, &format!("aux.{index}.type"))?,
            &kind,
            "stack_auxv",
            "reported auxv type",
        )?;
        same(
            &number(&native, &format!("aux.{index}.value"))?,
            &value,
            "stack_auxv",
            "reported auxv value",
        )?;
    }
    let native_maps = unhex(field(&native, "maps")?)?;
    let loaded_maps = unhex(field(&loaded, "maps")?)?;
    maps(&native_maps, &loaded_maps, pair)?;
    let parsed = parse_maps(&native_maps)?;
    require(
        parsed.iter().any(|mapping| {
            mapping.name == "[heap]" && mapping.start == pair.artifacts.prepared.layout.start_brk
        }),
        "heap_mapping",
        "native reporter lacks its heap at start_brk",
    )?;
    let libc_base = number(&native, "libc.base")?;
    require(
        parsed
            .iter()
            .any(|mapping| mapping.name.contains("libc") && mapping.start == libc_base),
        "libc_base",
        "reported libc base is absent from maps",
    )?;
    same(
        &number(&native, "program.base")?,
        &pair.artifacts.prepared.layout.load_bias,
        "program_base",
        "reported program load bias",
    )?;
    same(
        &number(&native, "heap.start")?,
        &pair.artifacts.prepared.layout.start_brk,
        "heap_metadata",
        "initial heap break",
    )?;
    let proc_native = decode_aux(&unhex(field(&native, "proc_auxv")?)?)
        .map_err(|error| failure("proc_auxv", error))?;
    let proc_loaded = decode_aux(&unhex(field(&loaded, "proc_auxv")?)?)
        .map_err(|error| failure("proc_auxv", error))?;
    same(
        &proc_loaded,
        &pair.loaded.origin.proc_aux,
        "proc_auxv",
        "reported loader proc auxv preserves kernel origin values",
    )?;
    for field in [26, 27] {
        same(
            &number(&loaded, &format!("stat.{field}"))?,
            &pair.loaded.origin.stat[&field],
            "proc_stat",
            &format!("reported loader code field{field} preserves kernel origin"),
        )?;
    }
    proc_auxv(
        &pair.native.entry.aux,
        &proc_native,
        &proc_loaded,
        &[3, 5, 7, 9, 33],
    )?;
    byte_same(
        &unhex(field(&native, "exe")?)?,
        &pair.native.entry.exe,
        "executable_identity",
        "reported native exe",
    )?;
    byte_same(
        &unhex(field(&loaded, "exe")?)?,
        &pair.loaded.entry.exe,
        "executable_identity",
        "reported loader exe",
    )?;
    if finite_data {
        require(
            number(&native, "heap.first_failed_request")? != 0
                && number(&native, "heap.growth_pages")? > 0
                && number(&native, "heap.growth_pages")?
                    < number(&native, "heap.probe_limit_pages")?,
            "finite_data_boundary",
            "probe did not reach a real RLIMIT_DATA heap failure",
        )?;
    } else {
        same(
            &number(&native, "heap.first_failed_request")?,
            &0,
            "heap_growth",
            "unlimited heap probe failure",
        )?;
        same(
            &number(&native, "heap.growth_pages")?,
            &number(&native, "heap.probe_limit_pages")?,
            "heap_growth",
            "successful heap probe pages",
        )?;
    }
    Ok(())
}

pub(super) fn observers(pair: &Pair) -> Check {
    observer_witness(&pair.native, "native")?;
    observer_witness(&pair.loaded, "loader")?;
    let native = &pair.native.stdout;
    let loaded = &pair.loaded.stdout;
    byte_same(
        native,
        loaded,
        "entry_registers",
        "full assembly entry observer record",
    )?;
    println!(
        "entry observer features={:#x} XCR0={:#x} record={}bytes",
        word(native, 160)?,
        word(native, 168)?,
        native.len()
    );
    Ok(())
}

pub(super) fn observer_witness(run: &super::trace::Run, mode: &str) -> Check {
    let record = &run.stdout;
    let registers = &run.entry.registers;
    require(
        record.len() >= 2368 && &record[..8] == b"ELFOBS01",
        "entry_observer",
        format!("{mode} observer magic/size is invalid"),
    )?;
    same(
        &word(record, 8)?,
        &2368,
        "entry_observer",
        &format!("{mode} fixed header length"),
    )?;
    same(
        &(record.len() as u64),
        &(2368 + word(record, 184)?),
        "entry_observer",
        &format!("{mode} total observer record length"),
    )?;
    for (index, value) in registers.gpr.iter().enumerate() {
        same(
            &word(record, 16 + index * 8)?,
            value,
            "entry_observer",
            &format!("independent {mode} GPR{index} witness"),
        )?;
    }
    same(
        &word(record, 144)?,
        &registers.rflags,
        "entry_observer",
        &format!("independent {mode} RFLAGS witness"),
    )?;
    same(
        &u32::from_le_bytes(record[152..156].try_into().expect("four bytes")),
        &registers.mxcsr,
        "entry_observer",
        &format!("independent {mode} MXCSR witness"),
    )?;
    same(
        &half(record, 156)?,
        &registers.x87_control,
        "entry_observer",
        &format!("independent {mode} x87 control word witness"),
    )?;
    same(
        &half(record, 158)?,
        &registers.x87_status,
        "entry_observer",
        &format!("independent {mode} x87 status word witness"),
    )?;
    same(
        &record[2308],
        &registers.x87_tag,
        "entry_observer",
        &format!("independent {mode} x87 abridged tag word witness"),
    )?;
    for (index, name) in ["CS", "SS", "DS", "ES", "FS", "GS"].into_iter().enumerate() {
        same(
            &u64::from(half(record, 2310 + index * 2)?),
            &registers.selectors[index],
            "entry_observer",
            &format!("independent {mode} {name} selector witness"),
        )?;
    }
    for (name, offset, value) in [
        ("FS", 2328, registers.fs_base),
        ("GS", 2336, registers.gs_base),
    ] {
        same(
            &word(record, offset)?,
            &value,
            "entry_observer",
            &format!("independent {mode} {name} base witness"),
        )?;
    }
    same(
        &word(record, 160)?,
        &registers.feature_mask,
        "entry_observer",
        &format!("{mode} advertised feature mask"),
    )?;
    same(
        &word(record, 168)?,
        &registers.xcr0,
        "entry_observer",
        &format!("{mode} XCR0"),
    )?;
    same(
        &word(record, 176)?,
        &run.entry.stack_start,
        "entry_observer",
        &format!("{mode} unmodified stack witness start"),
    )?;
    byte_same(
        &record[2368..],
        &run.entry.stack,
        "entry_observer",
        &format!("independent {mode} original-stack witness"),
    )?;
    byte_same(
        &record[192..448],
        &registers.xmm,
        "entry_observer",
        &format!("independent {mode} XMM witness"),
    )?;
    for (name, start, length) in [
        ("ymm_upper", 448, 256),
        ("zmm_upper", 704, 512),
        ("zmm16_31", 1216, 1024),
        ("opmask", 2240, 64),
        ("pkru", 2304, 4),
    ] {
        if let Some(bytes) = registers.extended.get(name) {
            byte_same(
                &record[start..start + length],
                bytes,
                "entry_observer",
                &format!("independent {mode} {name} witness"),
            )?;
        }
    }
    Ok(())
}

fn byte_same(native: &[u8], loaded: &[u8], assertion: &'static str, field: &str) -> Check {
    if let Some(index) = native
        .iter()
        .zip(loaded)
        .position(|(left, right)| left != right)
    {
        return Err(failure(
            assertion,
            format!(
                "{field}: first differing byte at{index:#x}, native={:#04x}, loader={:#04x}",
                native[index], loaded[index]
            ),
        ));
    }
    same(
        &native.len(),
        &loaded.len(),
        assertion,
        &format!("{field} length"),
    )
}

fn report(bytes: &[u8]) -> Check<BTreeMap<String, String>> {
    let text =
        std::str::from_utf8(bytes).map_err(|error| failure("layout_output", error.to_string()))?;
    let mut result = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| failure("layout_output", "reporter line has no '='"))?;
        require(
            result.insert(key.to_string(), value.to_string()).is_none(),
            "layout_output",
            format!("duplicate reporter key{key}"),
        )?;
    }
    Ok(result)
}

fn field<'a>(report: &'a BTreeMap<String, String>, key: &str) -> Check<&'a str> {
    report.get(key).map(String::as_str).ok_or_else(|| {
        failure(
            "layout_output",
            format!("required reporter key{key} is absent"),
        )
    })
}

fn number(report: &BTreeMap<String, String>, key: &str) -> Check<u64> {
    let value = field(report, key)?;
    if let Some(hex) = value.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)
    } else {
        value.parse::<u64>()
    }
    .map_err(|error| failure("layout_output", format!("{key}: {error}")))
}

fn unhex(value: &str) -> Check<Vec<u8>> {
    require(
        value.len().is_multiple_of(2),
        "layout_output",
        "hex data has an odd length",
    )?;
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| {
            let text = std::str::from_utf8(bytes)
                .map_err(|error| failure("layout_output", error.to_string()))?;
            u8::from_str_radix(text, 16)
                .map_err(|error| failure("layout_output", error.to_string()))
        })
        .collect()
}

fn word(bytes: &[u8], offset: usize) -> Check<u64> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or_else(|| failure("entry_observer", "word is outside record"))?
            .try_into()
            .expect("eight bytes"),
    ))
}

fn half(bytes: &[u8], offset: usize) -> Check<u16> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or_else(|| failure("entry_observer", "halfword is outside record"))?
            .try_into()
            .expect("two bytes"),
    ))
}
