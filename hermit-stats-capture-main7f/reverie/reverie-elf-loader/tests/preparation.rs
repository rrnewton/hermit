/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

use reverie_elf_loader::AUXV_PATCH_TYPES;
use reverie_elf_loader::AuxvValue;
use reverie_elf_loader::Error;
use reverie_elf_loader::Invocation;
use reverie_elf_loader::Limits;
use reverie_elf_loader::MIN_PROGRAM_ADDRESS;
use reverie_elf_loader::PATH_MAX;
use reverie_elf_loader::pad_launcher_path;
use reverie_elf_loader::prepare_start;
use reverie_elf_loader::prepare_start_with_limits;
use sha2::Digest;
use sha2::Sha256;

const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_NOTE: u32 = 4;
const PT_GNU_STACK: u32 = 0x6474e551;

fn timed(name: &str, work: impl FnOnce()) {
    let start = Instant::now();
    work();
    let path = Path::new(env!("ELF_LOADER_ARTIFACT_DIR")).join(format!("{name}.result"));
    fs::write(
        path,
        format!("PASS {name} {:.6}s\n", start.elapsed().as_secs_f64()),
    )
    .unwrap();
}

fn fixture() -> File {
    File::open(env!("ELF_LOADER_LAYOUT_NONPIE")).unwrap()
}

#[test]
fn pie_zero_alignment_unaligned_first_load() {
    timed("pie_zero_alignment_unaligned_first_load", || {
        assert_unaligned_pie_layout("zero", None, 0x555555554000);
    });
}

#[test]
fn pie_mixed_alignment_unaligned_first_load() {
    timed("pie_mixed_alignment_unaligned_first_load", || {
        for (alignment, expected_bias) in [
            (1, 0x555555553000),
            (2, 0x555555553000),
            (4096, 0x555555553000),
            (0x200000, 0x5555553ff000),
        ] {
            assert_unaligned_pie_layout(
                &format!("mixed-{alignment:x}"),
                Some(alignment),
                expected_bias,
            );
        }
    });
}

fn assert_unaligned_pie_layout(name: &str, second_alignment: Option<u64>, expected_bias: u64) {
    let source = fs::read(env!("ELF_LOADER_ENTRY_PIE")).unwrap();
    let mut image = source[..64].to_vec();
    let load_count = if second_alignment.is_some() { 2 } else { 1 };
    image[16..18].copy_from_slice(&3_u16.to_le_bytes());
    image[24..32].copy_from_slice(&0x123_u64.to_le_bytes());
    image[32..40].copy_from_slice(&0x123_u64.to_le_bytes());
    image[40..48].fill(0);
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
        image[at..at + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        image[at + 4..at + 8].copy_from_slice(&flags.to_le_bytes());
        for (field, value) in [
            (8, address),
            (16, address),
            (24, address),
            (32, 0x200),
            (40, 0x200),
            (48, alignment),
        ] {
            image[at + field..at + field + 8].copy_from_slice(&value.to_le_bytes());
        }
    }
    let name_bytes = env!("ELF_LOADER_OBSERVER").as_bytes();
    let interp = 0x123 + load_count * 56;
    let offset = image.len() as u64;
    image[interp..interp + 4].copy_from_slice(&PT_INTERP.to_le_bytes());
    image[interp + 8..interp + 16].copy_from_slice(&offset.to_le_bytes());
    image[interp + 32..interp + 40].copy_from_slice(&(name_bytes.len() as u64 + 1).to_le_bytes());
    image[interp + 40..interp + 48].copy_from_slice(&(name_bytes.len() as u64 + 1).to_le_bytes());
    image.extend_from_slice(name_bytes);
    image.push(0);
    let stack = interp + 56;
    image[stack..stack + 4].copy_from_slice(&PT_GNU_STACK.to_le_bytes());
    image[stack + 4..stack + 8].copy_from_slice(&6_u32.to_le_bytes());
    image[stack + 48..stack + 56].copy_from_slice(&16_u64.to_le_bytes());
    if load_count == 1 {
        image[stack + 56..stack + 60].copy_from_slice(&PT_NOTE.to_le_bytes());
    }
    let target = write_preparation_fixture(
        &format!("unaligned-pie-{name}.elf"),
        &image,
        &format!(
            "ET_DYN; first offset=vaddr=phoff=entry=0x123, filesz=memsz=0x200, align=0; second align={second_alignment:#x?}"
        ),
    );
    let prepared = prepare_start(
        &target,
        &Invocation::execve("./unaligned-pie").unwrap(),
        Path::new("./h"),
    )
    .unwrap();
    println!(
        "unaligned PIE {name}: expected bias={expected_bias:#x}, prepared bias={:#x}",
        prepared.layout.load_bias
    );
    assert_eq!(prepared.layout.load_bias, expected_bias);
    assert_eq!(prepared.layout.program_base, expected_bias);
    assert_eq!(prepared.layout.phdr, expected_bias + 0x123);
    assert_eq!(prepared.layout.entry, expected_bias + 0x123);
    assert_eq!(prepared.layout.phnum, 4);
    let last_address = if load_count == 2 { 0x2123 } else { 0x123 };
    assert_eq!(prepared.layout.start_data, expected_bias + last_address);
    assert_eq!(
        prepared.layout.end_data,
        expected_bias + last_address + 0x200
    );
    assert_eq!(
        prepared.layout.start_brk,
        expected_bias + if load_count == 2 { 0x3000 } else { 0x1000 }
    );
    assert_eq!(
        prepared.auxv_patches[0].value,
        AuxvValue::Fixed(expected_bias + 0x123)
    );
    assert_eq!(
        prepared.auxv_patches[3].value,
        AuxvValue::Fixed(expected_bias + 0x123)
    );
}

#[test]
fn padded_path_path_max_boundary() {
    timed("padded_path_path_max_boundary", || {
        let path = pad_launcher_path(Path::new("./h"), PATH_MAX - 1).unwrap();
        assert_eq!(path.as_bytes_with_nul().len(), PATH_MAX);
        assert!(matches!(
            pad_launcher_path(Path::new("./h"), PATH_MAX),
            Err(Error::PaddedPathTooLong {
                length_with_nul: 4097,
                maximum: 4096
            })
        ));
        // A legal 4095-byte relative path produces a longer kernel execfn when
        // execveat prefixes it with its directory descriptor.
        let relative = format!(".{}fixture", "/".repeat(PATH_MAX - 1 - 1 - 7));
        assert_eq!(relative.len(), PATH_MAX - 1);
        let invocation = Invocation::execveat(12, relative, 0).unwrap();
        let filename_length = invocation.native_execfn().as_bytes().len();
        assert_eq!(filename_length, PATH_MAX - 1 + "/dev/fd/12/".len());
        assert!(matches!(
            prepare_start(&fixture(), &invocation, Path::new("./h")),
            Err(Error::PaddedPathTooLong {
                length_with_nul: 4107,
                maximum: 4096
            })
        ));
        let invocation =
            Invocation::execve(format!(".{}fixture", "/".repeat(PATH_MAX - 9))).unwrap();
        assert_eq!(
            prepare_start(&fixture(), &invocation, Path::new("./h"))
                .unwrap()
                .padded_path
                .as_bytes_with_nul()
                .len(),
            PATH_MAX
        );
    });
}

#[test]
fn program_load_address_boundary() {
    timed("program_load_address_boundary", || {
        let invocation = Invocation::execve("./fixture").unwrap();
        let accepted = prepare_start(&fixture(), &invocation, Path::new("./h")).unwrap();
        assert_eq!(accepted.layout.program_base, MIN_PROGRAM_ADDRESS);
        let rejected = File::open(env!("ELF_LOADER_LOW_TARGET")).unwrap();
        assert!(matches!(
            prepare_start(&rejected, &invocation, Path::new("./h")),
            Err(Error::LowLoadSegment {
                address: 0x3ff000,
                interpreter: false
            })
        ));
        let pie = File::open(env!("ELF_LOADER_LAYOUT_PIE")).unwrap();
        let accepted = prepare_start(&pie, &invocation, Path::new("./h")).unwrap();
        assert_eq!(accepted.layout.program_base, 0x555555554000);
    });
}

#[test]
fn invocation_execfn_and_auxv_patch_contract() {
    timed("invocation_execfn_and_auxv_patch_contract", || {
        assert_eq!(
            Invocation::execve("./fixture")
                .unwrap()
                .native_execfn()
                .as_bytes(),
            b"./fixture"
        );
        assert_eq!(
            Invocation::execveat(12, "fixture", 0)
                .unwrap()
                .native_execfn()
                .as_bytes(),
            b"/dev/fd/12/fixture"
        );
        assert_eq!(
            Invocation::execveat(12, "", libc::AT_EMPTY_PATH)
                .unwrap()
                .native_execfn()
                .as_bytes(),
            b"/dev/fd/12"
        );
        assert_eq!(
            Invocation::execveat(12, "/bin/true", 0)
                .unwrap()
                .native_execfn()
                .as_bytes(),
            b"/bin/true"
        );
        let invocation = Invocation::execve("./custom-name").unwrap();
        let prepared = prepare_start(&fixture(), &invocation, Path::new("./h")).unwrap();
        assert_eq!(prepared.comm.as_bytes(), b"custom-name");
        let scalar_types: Vec<_> = prepared
            .auxv_patches
            .iter()
            .filter(|p| p.value != AuxvValue::ExecFnString)
            .map(|p| p.kind)
            .collect();
        assert_eq!(scalar_types, AUXV_PATCH_TYPES);
        assert_eq!(prepared.metadata(), b"./custom-name\0custom-name\0\0");
        assert!(matches!(
            Invocation::execveat(12, "", 0),
            Err(Error::InvalidInvocation(_))
        ));
        assert!(matches!(
            Invocation::execve("a\0b"),
            Err(Error::InvalidInvocation(_))
        ));
    });
}

#[test]
fn finite_address_space_refusal_boundary() {
    timed("finite_address_space_refusal_boundary", || {
        let invocation = Invocation::execve("./fixture").unwrap();
        let limits = Limits {
            data: libc::RLIM_INFINITY,
            address_space: libc::RLIM_INFINITY,
        };
        prepare_start_with_limits(&fixture(), &invocation, Path::new("./h"), limits).unwrap();
        for limit in [0, 4 << 20, libc::RLIM_INFINITY - 1] {
            let limits = Limits {
                address_space: limit,
                ..limits
            };
            assert!(
                matches!(prepare_start_with_limits(&fixture(), &invocation, Path::new("./h"), limits),
                Err(Error::FiniteAddressSpaceLimit { limit: observed }) if observed == limit)
            );
        }
        // DATA is admitted even when finite; execution/accounting parity is
        // exercised by the syscall-traced finite-limit integration test.
        prepare_start_with_limits(
            &fixture(),
            &invocation,
            Path::new("./h"),
            Limits {
                data: 4 << 20,
                ..limits
            },
        )
        .unwrap();
    });
}

#[test]
fn shadow_image_is_sparse_and_carries_native_data_fields() {
    timed(
        "shadow_image_is_sparse_and_carries_native_data_fields",
        || {
            let invocation = Invocation::execve("./fixture").unwrap();
            let prepared = prepare_start(&fixture(), &invocation, Path::new("./h")).unwrap();
            let target = fs::read(env!("ELF_LOADER_LAYOUT_NONPIE")).unwrap();
            let target_phoff = u64::from_le_bytes(target[32..40].try_into().unwrap()) as usize;
            let target_phnum = u16::from_le_bytes(target[56..58].try_into().unwrap()) as usize;
            let raw_end = (0..target_phnum)
                .map(|index| &target[target_phoff + index * 56..target_phoff + (index + 1) * 56])
                .filter(|header| u32::from_le_bytes(header[..4].try_into().unwrap()) == PT_LOAD)
                .map(|header| {
                    u64::from_le_bytes(header[16..24].try_into().unwrap())
                        + u64::from_le_bytes(header[40..48].try_into().unwrap())
                })
                .max()
                .unwrap()
                + prepared.layout.load_bias;
            assert_eq!(prepared.layout.memory_end, raw_end);
            assert_eq!(prepared.layout.start_brk, (raw_end + 4095) & !4095);
            let path = scratch("shadow-image.elf");
            let output = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            prepared.write_image(&output).unwrap();
            record_binary(
                &path,
                "PreparedStart::write_image; source=layout-nonpie; invocation=./fixture",
            );
            let bytes = fs::read(&path).unwrap();
            let phoff = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
            let phnum = u16::from_le_bytes(bytes[56..58].try_into().unwrap()) as usize;
            let headers: Vec<_> = (0..phnum)
                .map(|index| &bytes[phoff + index * 56..phoff + (index + 1) * 56])
                .collect();
            let shadow = headers
                .iter()
                .find(|header| {
                    u32::from_le_bytes(header[..4].try_into().unwrap()) == 1
                        && u64::from_le_bytes(header[16..24].try_into().unwrap())
                            == prepared.layout.start_data
                })
                .unwrap();
            assert_eq!(u32::from_le_bytes(shadow[4..8].try_into().unwrap()), 4);
            assert_eq!(
                u64::from_le_bytes(shadow[32..40].try_into().unwrap()),
                prepared.layout.end_data - prepared.layout.start_data
            );
            assert_eq!(
                u64::from_le_bytes(shadow[40..48].try_into().unwrap()),
                raw_end - prepared.layout.start_data
            );
            let offset = u64::from_le_bytes(shadow[8..16].try_into().unwrap()) as usize;
            assert!(bytes[offset..].iter().all(|byte| *byte == 0));
            assert_eq!(offset % 4096, prepared.layout.start_data as usize % 4096);
            use std::os::unix::fs::MetadataExt;
            assert!(output.metadata().unwrap().blocks() * 512 < output.metadata().unwrap().len());
            assert!(matches!(
                prepared.write_image_from_template(&output, b"not an ELF"),
                Err(Error::InvalidLoaderTemplate(_))
            ));
        },
    );
}

#[test]
fn malformed_and_static_elf_refusals() {
    timed("malformed_and_static_elf_refusals", || {
        let invocation = Invocation::execve("./fixture").unwrap();
        let source = fs::read(env!("ELF_LOADER_LAYOUT_NONPIE")).unwrap();
        let path = scratch("malformed.elf");
        for (offset, value, expected) in [(4, 1, "class"), (54, 55, "phsize")] {
            let mut image = source.clone();
            image[offset] = value;
            fs::write(&path, image).unwrap();
            record_binary(
                &path,
                &format!("layout-nonpie; ELF header byte[{offset}]={value}"),
            );
            assert!(
                matches!(
                    prepare_start(&File::open(&path).unwrap(), &invocation, Path::new("./h")),
                    Err(Error::UnsupportedElf(_))
                ),
                "{expected}"
            );
        }
        let mut image = source;
        let phoff = u64::from_le_bytes(image[32..40].try_into().unwrap()) as usize;
        let phnum = u16::from_le_bytes(image[56..58].try_into().unwrap()) as usize;
        for index in 0..phnum {
            let at = phoff + index * 56;
            if u32::from_le_bytes(image[at..at + 4].try_into().unwrap()) == 3 {
                image[at..at + 4].copy_from_slice(&0_u32.to_le_bytes());
            }
        }
        let mut output = File::create(&path).unwrap();
        output.write_all(&image).unwrap();
        record_binary(
            &path,
            "layout-nonpie; change every PT_INTERP p_type to PT_NULL",
        );
        assert!(matches!(
            prepare_start(&File::open(&path).unwrap(), &invocation, Path::new("./h")),
            Err(Error::MissingInterpreter)
        ));
    });
}

#[test]
fn descriptor_comm_ambiguity_is_named() {
    timed("descriptor_comm_ambiguity_is_named", || {
        let invocation = Invocation::execveat(12, "", libc::AT_EMPTY_PATH).unwrap();
        let path = scratch("x (deleted)");
        fs::copy(env!("ELF_LOADER_LAYOUT_NONPIE"), &path).unwrap();
        record_binary(&path, "copy layout-nonpie as x (deleted)");
        let pinned = File::open(&path).unwrap();
        assert!(matches!(
            prepare_start(&pinned, &invocation, Path::new("./h")),
            Err(Error::AmbiguousDescriptorComm)
        ));
        fs::remove_file(&path).unwrap();
        let accepted = prepare_start(&pinned, &invocation, Path::new("./h")).unwrap();
        assert_eq!(accepted.comm.as_bytes(), b"x (deleted)");

        let path = scratch("linked-live");
        let alias = scratch("linked-alias");
        if alias.exists() {
            fs::remove_file(&alias).unwrap();
        }
        fs::copy(env!("ELF_LOADER_LAYOUT_NONPIE"), &path).unwrap();
        fs::hard_link(&path, &alias).unwrap();
        record_binary(
            &path,
            "copy layout-nonpie as linked-live; hard_link linked-alias",
        );
        let pinned = File::open(&path).unwrap();
        let accepted = prepare_start(&pinned, &invocation, Path::new("./h")).unwrap();
        assert_eq!(accepted.comm.as_bytes(), b"linked-live");
        fs::remove_file(path).unwrap();
        assert!(matches!(
            prepare_start(&pinned, &invocation, Path::new("./h")),
            Err(Error::AmbiguousDescriptorComm)
        ));
        fs::remove_file(alias).unwrap();
        let accepted = prepare_start(&pinned, &invocation, Path::new("./h")).unwrap();
        assert_eq!(accepted.comm.as_bytes(), b"linked-live");
    });
}

#[test]
fn single_nonexecutable_stack_header_is_accepted() {
    timed("single_nonexecutable_stack_header_is_accepted", || {
        let invocation = Invocation::execve("./fixture").unwrap();
        let source = fs::read(env!("ELF_LOADER_LAYOUT_NONPIE")).unwrap();
        let stacks = program_header_offsets(&source, PT_GNU_STACK);
        assert_eq!(stacks.len(), 1);
        assert_eq!(
            u32::from_le_bytes(source[stacks[0] + 4..stacks[0] + 8].try_into().unwrap()),
            6
        );
        prepare_start(&fixture(), &invocation, Path::new("./h")).unwrap();
    });
}

#[test]
fn single_executable_stack_header_is_refused() {
    timed("single_executable_stack_header_is_refused", || {
        let mut image = fs::read(env!("ELF_LOADER_LAYOUT_NONPIE")).unwrap();
        let stacks = program_header_offsets(&image, PT_GNU_STACK);
        assert_eq!(stacks.len(), 1);
        let at = stacks[0];
        image[at + 4..at + 8].copy_from_slice(&7_u32.to_le_bytes());
        assert_eq!(program_header_offsets(&image, PT_GNU_STACK).len(), 1);
        let target = write_preparation_fixture(
            "single-executable-stack.elf",
            &image,
            "layout-nonpie; sole PT_GNU_STACK changes flags from RW to RWX",
        );
        assert!(matches!(
            prepare_start(
                &target,
                &Invocation::execve("./fixture").unwrap(),
                Path::new("./h")
            ),
            Err(Error::ExecutableStack)
        ));
    });
}

#[test]
fn missing_stack_header_is_refused() {
    timed("missing_stack_header_is_refused", || {
        let mut image = fs::read(env!("ELF_LOADER_LAYOUT_NONPIE")).unwrap();
        let stacks = program_header_offsets(&image, PT_GNU_STACK);
        assert_eq!(stacks.len(), 1);
        let at = stacks[0];
        image[at..at + 4].copy_from_slice(&0_u32.to_le_bytes());
        assert!(program_header_offsets(&image, PT_GNU_STACK).is_empty());
        let target = write_preparation_fixture(
            "missing-stack.elf",
            &image,
            "layout-nonpie; sole PT_GNU_STACK becomes PT_NULL",
        );
        assert!(matches!(
            prepare_start(
                &target,
                &Invocation::execve("./fixture").unwrap(),
                Path::new("./h")
            ),
            Err(Error::ExecutableStack)
        ));
    });
}

#[test]
fn duplicate_stack_header_is_refused() {
    timed("duplicate_stack_header_is_refused", || {
        let invocation = Invocation::execve("./fixture").unwrap();
        let source = fs::read(env!("ELF_LOADER_LAYOUT_NONPIE")).unwrap();
        assert_eq!(program_header_offsets(&source, PT_GNU_STACK).len(), 1);
        let at = program_header_offsets(&source, PT_NOTE)[0];
        for flags in [6_u32, 7] {
            let mut image = source.clone();
            image[at..at + 4].copy_from_slice(&PT_GNU_STACK.to_le_bytes());
            image[at + 4..at + 8].copy_from_slice(&flags.to_le_bytes());
            let stacks = program_header_offsets(&image, PT_GNU_STACK);
            assert_eq!(stacks.len(), 2);
            if flags == 6 {
                assert!(stacks.iter().all(|at| {
                    u32::from_le_bytes(image[at + 4..at + 8].try_into().unwrap()) & 1 == 0
                }));
            }
            let target = write_preparation_fixture(
                &format!("duplicate-stack-flags-{flags}.elf"),
                &image,
                &format!("layout-nonpie; first PT_NOTE becomes PT_GNU_STACK flags={flags}"),
            );
            assert!(matches!(
                prepare_start(&target, &invocation, Path::new("./h")),
                Err(Error::ExecutableStack)
            ));
        }
    });
}

#[test]
fn last_header_page_mapping_supplies_phdr_and_auxv_patch() {
    timed(
        "last_header_page_mapping_supplies_phdr_and_auxv_patch",
        || {
            let source = fs::read(env!("ELF_LOADER_LAYOUT_NONPIE")).unwrap();
            let phoff = u64::from_le_bytes(source[32..40].try_into().unwrap());
            assert!(phoff < 4096);
            let loads = program_header_offsets(&source, PT_LOAD);
            let first = loads[0];
            assert_eq!(
                u64::from_le_bytes(source[first + 8..first + 16].try_into().unwrap()),
                0
            );
            let high = loads
                .iter()
                .map(|at| {
                    u64::from_le_bytes(source[at + 16..at + 24].try_into().unwrap())
                        + u64::from_le_bytes(source[at + 40..at + 48].try_into().unwrap())
                })
                .max()
                .unwrap();
            let duplicate_address = (high + 4095) & !4095;
            let at = program_header_offsets(&source, PT_NOTE)
                .into_iter()
                .find(|at| *at > *loads.last().unwrap())
                .unwrap();
            let mut image = source.clone();
            image[at..at + 56].copy_from_slice(&source[first..first + 56]);
            image[at + 4..at + 8].copy_from_slice(&4_u32.to_le_bytes());
            image[at + 16..at + 24].copy_from_slice(&duplicate_address.to_le_bytes());
            image[at + 24..at + 32].copy_from_slice(&duplicate_address.to_le_bytes());
            image[at + 32..at + 40].copy_from_slice(&4096_u64.to_le_bytes());
            image[at + 40..at + 48].copy_from_slice(&4096_u64.to_le_bytes());
            let containing_headers: Vec<_> = program_header_offsets(&image, PT_LOAD)
                .into_iter()
                .filter(|at| {
                    let offset = u64::from_le_bytes(image[at + 8..at + 16].try_into().unwrap());
                    let size = u64::from_le_bytes(image[at + 32..at + 40].try_into().unwrap());
                    offset <= phoff && phoff < offset + size
                })
                .collect();
            assert_eq!(containing_headers, [first, at]);
            let target = write_preparation_fixture(
                "duplicate-header-page.elf",
                &image,
                &format!(
                    "layout-nonpie; PT_NOTE after last PT_LOAD becomes R-only PT_LOAD of header page at {duplicate_address:#x}"
                ),
            );
            let prepared = prepare_start(
                &target,
                &Invocation::execve("./fixture").unwrap(),
                Path::new("./h"),
            )
            .unwrap();
            let expected = duplicate_address + phoff;
            assert_eq!(prepared.layout.load_bias, 0);
            assert_eq!(prepared.layout.phdr, expected);
            let phdr_patches: Vec<_> = prepared
                .auxv_patches
                .iter()
                .filter(|patch| patch.kind == libc::AT_PHDR)
                .map(|patch| patch.value)
                .collect();
            assert_eq!(phdr_patches, [AuxvValue::Fixed(expected)]);
        },
    );
}

#[test]
fn zero_interpreter_load_span_is_refused() {
    timed("zero_interpreter_load_span_is_refused", || {
        for kind in [2_u16, 3] {
            let image = minimal_interpreter(kind, 0x800000, 0);
            let target = target_with_interpreter(
                &format!("zero-span-{kind}"),
                &image,
                &format!("minimal e_type={kind} interpreter; aligned PT_LOAD filesz=memsz=0"),
            );
            assert!(matches!(
                prepare_start(
                    &target,
                    &Invocation::execve("./fixture").unwrap(),
                    Path::new("./h")
                ),
                Err(Error::ZeroInterpreterLoadSpan)
            ));
        }
    });
}

#[test]
fn positive_interpreter_load_spans_are_accepted() {
    timed("positive_interpreter_load_spans_are_accepted", || {
        for kind in [2_u16, 3] {
            // Both have a one-byte total_mapping_size. The second has no
            // memory payload, but its unaligned vaddr still gives a span.
            for (address, memsize) in [(0x800000, 1), (0x800001, 0)] {
                let image = minimal_interpreter(kind, address, memsize);
                let target = target_with_interpreter(
                    &format!("positive-span-{kind}-{address:x}-{memsize}"),
                    &image,
                    &format!(
                        "minimal e_type={kind} interpreter; PT_LOAD vaddr={address:#x} filesz=0 memsz={memsize}; mapping span=1"
                    ),
                );
                prepare_start(
                    &target,
                    &Invocation::execve("./fixture").unwrap(),
                    Path::new("./h"),
                )
                .unwrap();
            }
        }
    });
}

#[test]
fn dynamic_interpreter_hint_reserved_range_boundary() {
    timed("dynamic_interpreter_hint_reserved_range_boundary", || {
        let invocation = Invocation::execve("./fixture").unwrap();
        let source = fs::read(env!("ELF_LOADER_LAYOUT_NONPIE")).unwrap();
        let mut main = source;
        let relocation = 0x1000000 - MIN_PROGRAM_ADDRESS;
        let entry = u64::from_le_bytes(main[24..32].try_into().unwrap()) + relocation;
        main[24..32].copy_from_slice(&entry.to_le_bytes());
        let phoff = u64::from_le_bytes(main[32..40].try_into().unwrap()) as usize;
        let phnum = u16::from_le_bytes(main[56..58].try_into().unwrap()) as usize;
        for index in 0..phnum {
            let at = phoff + index * 56;
            for field in [16, 24] {
                let address =
                    u64::from_le_bytes(main[at + field..at + field + 8].try_into().unwrap());
                if address != 0 {
                    main[at + field..at + field + 8]
                        .copy_from_slice(&(address + relocation).to_le_bytes());
                }
            }
        }
        // Both boundary spans are below this relocated main image. A main at
        // the ordinary 0x400000 base would occupy the admitted hint itself.
        for hint in [0x100000, MIN_PROGRAM_ADDRESS - 4096] {
            let interpreter = file_backed_interpreter(3, hint);
            let target = target_with_interpreter_from_main(
                &format!("reserved-dynamic-hint-{hint:x}"),
                &interpreter,
                &format!("ET_DYN interpreter; file-backed first PT_LOAD hint={hint:#x}; span=4096"),
                main.clone(),
            );
            assert!(matches!(
                prepare_start(&target, &invocation, Path::new("./h")),
                Err(Error::InterpreterHintOverlapsLoader {
                    address,
                    span: 4096
                }) if address == hint
            ));
        }
        for hint in [0, MIN_PROGRAM_ADDRESS] {
            let interpreter = file_backed_interpreter(3, hint);
            let target = target_with_interpreter_from_main(
                &format!("admitted-dynamic-hint-{hint:x}"),
                &interpreter,
                &format!("ET_DYN interpreter; file-backed first PT_LOAD hint={hint:#x}; span=4096"),
                main.clone(),
            );
            let prepared = prepare_start(&target, &invocation, Path::new("./h")).unwrap();
            assert_eq!(prepared.layout.load_bias, 0);
            assert_eq!(prepared.layout.program_base, 0x1000000);
        }
        let interpreter = file_backed_interpreter(3, 0x100000);
        let target = target_with_interpreter_from_main(
            "pie-reserved-dynamic-hint",
            &interpreter,
            "ET_DYN interpreter; file-backed first PT_LOAD hint=0x100000; biased PIE main discards hint",
            fs::read(env!("ELF_LOADER_LAYOUT_PIE")).unwrap(),
        );
        let prepared = prepare_start(&target, &invocation, Path::new("./h")).unwrap();
        assert_ne!(prepared.layout.load_bias, 0);
    });
}

#[test]
fn interpreter_effective_load_range_boundaries() {
    timed("interpreter_effective_load_range_boundaries", || {
        for (source, pie) in [
            (env!("ELF_LOADER_LAYOUT_NONPIE"), false),
            (env!("ELF_LOADER_LAYOUT_PIE"), true),
        ] {
            for kind in [2_u16, 3] {
                for first_address in [0, 0x123] {
                    for address in [0x100000, 0x3ff000, 0x400000] {
                        for file_backed in [false, true] {
                            let interpreter = mixed_interpreter(
                                kind,
                                &[
                                    (first_address, 0, 0),
                                    (address, u64::from(file_backed) * 4096, 4096),
                                ],
                            );
                            let target = target_with_interpreter_from_main(
                                &format!(
                                    "mixed-range-{pie}-{kind}-{first_address:x}-{address:x}-{file_backed}"
                                ),
                                &interpreter,
                                "empty first interpreter load followed by file/BSS range at independent boundary",
                                fs::read(source).unwrap(),
                            );
                            let result = prepare_start(
                                &target,
                                &Invocation::execve("./fixture").unwrap(),
                                Path::new("./h"),
                            );
                            if address == MIN_PROGRAM_ADDRESS {
                                result.unwrap();
                            } else {
                                assert!(matches!(
                                    result,
                                    Err(Error::LowLoadSegment { address: observed, interpreter: true })
                                        if observed == address
                                ));
                            }
                        }
                    }
                }
            }
            // A later empty load in the low band also maps nothing. The
            // nonempty last load alone determines the reserved-range result.
            let interpreter =
                mixed_interpreter(3, &[(0, 0, 0), (0x200123, 0, 0), (0x400000, 4096, 4096)]);
            let target = target_with_interpreter_from_main(
                &format!("later-empty-{pie}"),
                &interpreter,
                "empty first and later low unaligned PT_LOADs; nonempty PT_LOAD starts at 0x400000",
                fs::read(source).unwrap(),
            );
            prepare_start(
                &target,
                &Invocation::execve("./fixture").unwrap(),
                Path::new("./h"),
            )
            .unwrap();
        }
        // These raw addresses are all above the band. PIE's empty first load
        // establishes -0x800000 bias, so the later ranges must be checked at
        // their effective 0x3ff000 and 0x400000 addresses.
        for (raw_address, effective, admitted) in
            [(0xbff000, 0x3ff000, false), (0xc00000, 0x400000, true)]
        {
            let mut interpreter =
                mixed_interpreter(3, &[(0x800123, 0, 0), (raw_address, 4096, 4096)]);
            // The empty first load establishes -0x800000 bias. Give this
            // geometry control a valid resolved entry at 0x400000 as well.
            interpreter[24..32].copy_from_slice(&0xc00000_u64.to_le_bytes());
            let target = target_with_interpreter_from_main(
                &format!("negative-bias-{raw_address:x}"),
                &interpreter,
                "PIE interpreter empty first load at 0x800123; later range resolved with -0x800000 bias",
                fs::read(env!("ELF_LOADER_LAYOUT_PIE")).unwrap(),
            );
            let result = prepare_start(
                &target,
                &Invocation::execve("./fixture").unwrap(),
                Path::new("./h"),
            );
            if admitted {
                result.unwrap();
            } else {
                assert!(
                    matches!(result, Err(Error::LowLoadSegment { address, interpreter: true }) if address == effective)
                );
            }
        }
    });
}

#[test]
fn zero_file_first_interpreter_bss_reserved_range() {
    timed("zero_file_first_interpreter_bss_reserved_range", || {
        for (source, raw_address, effective) in [
            (env!("ELF_LOADER_LAYOUT_NONPIE"), 0x100000, 0x100000),
            (env!("ELF_LOADER_LAYOUT_NONPIE"), 0, 0),
            (env!("ELF_LOADER_LAYOUT_PIE"), 0x800000, 0),
        ] {
            let interpreter = minimal_interpreter(3, raw_address, 4096);
            let target = target_with_interpreter_from_main(
                &format!("first-bss-{raw_address:x}-{effective:x}"),
                &interpreter,
                "zero-filesz nonempty first interpreter PT_LOAD; BSS is fixed without an allocator-selected base",
                fs::read(source).unwrap(),
            );
            assert!(matches!(
                prepare_start(&target, &Invocation::execve("./fixture").unwrap(), Path::new("./h")),
                Err(Error::LowLoadSegment { address, interpreter: true }) if address == effective
            ));
        }
    });
}

#[test]
fn caller_modified_shadow_cannot_overlap_loader() {
    timed("caller_modified_shadow_cannot_overlap_loader", || {
        let prepared = prepare_start(
            &fixture(),
            &Invocation::execve("./fixture").unwrap(),
            Path::new("./h"),
        )
        .unwrap();
        let path = scratch("shadow-reserved-refusal.elf");
        fs::write(&path, b"unchanged output").unwrap();
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        for address in [0x100000, 0x200000, 0x3ff000] {
            let mut modified = prepared.clone();
            modified.layout.start_data = address;
            modified.layout.end_data = address + 128;
            modified.layout.memory_end = address + 4096;
            modified.layout.start_brk = address + 4096;
            assert!(
                matches!(modified.write_image(&output), Err(Error::LowLoadSegment { address: observed, interpreter: false }) if observed == address)
            );
            assert_eq!(fs::read(&path).unwrap(), b"unchanged output");
        }
        let mut modified = prepared.clone();
        modified.layout.start_brk += 4096;
        assert!(matches!(
            modified.write_image(&output),
            Err(Error::InvalidElf("start_brk is not the rounded memory end"))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"unchanged output");
    });
}

#[test]
fn raw_and_biased_user_address_bounds() {
    timed("raw_and_biased_user_address_bounds", || {
        for (source, raw, reason) in [
            (
                env!("ELF_LOADER_LAYOUT_NONPIE"),
                true,
                "PT_LOAD exceeds x86-64 default user address range",
            ),
            (
                env!("ELF_LOADER_LAYOUT_PIE"),
                false,
                "biased PT_LOAD exceeds x86-64 default user address range",
            ),
        ] {
            let mut image = fs::read(source).unwrap();
            let phoff = u64::from_le_bytes(image[32..40].try_into().unwrap()) as usize;
            let phnum = u16::from_le_bytes(image[56..58].try_into().unwrap()) as usize;
            let last_load = (0..phnum)
                .rfind(|index| {
                    u32::from_le_bytes(
                        image[phoff + index * 56..phoff + index * 56 + 4]
                            .try_into()
                            .unwrap(),
                    ) == 1
                })
                .unwrap();
            let at = phoff + last_load * 56;
            let original = u64::from_le_bytes(image[at + 16..at + 24].try_into().unwrap());
            let address = if raw {
                0x800000000000_u64
            } else {
                // Keep the raw range below the initial stack reservation
                // and reserved top page; its PIE bias alone exceeds USER_LIMIT.
                0x7fffff000000_u64
            } + original % 4096;
            image[at + 16..at + 24].copy_from_slice(&address.to_le_bytes());
            if !raw {
                let size = 8192 - original % 4096;
                image[at + 40..at + 48].copy_from_slice(&size.to_le_bytes());
            }
            let path = scratch(&format!("address-bound-{raw}.elf"));
            fs::write(&path, image).unwrap();
            record_binary(
                &path,
                &format!("source={source}; last PT_LOAD vaddr={address:#x}; raw={raw}"),
            );
            assert!(
                matches!(prepare_start(&File::open(path).unwrap(), &Invocation::execve("./fixture").unwrap(), Path::new("./h")),
                Err(Error::UnsupportedElf(observed)) if observed == reason)
            );
        }
    });
}

#[test]
fn nonregular_interpreter_is_refused_without_opening_it_for_read() {
    timed(
        "nonregular_interpreter_is_refused_without_opening_it_for_read",
        || {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            let fifo = scratch("interpreter.fifo");
            if fifo.exists() {
                fs::remove_file(&fifo).unwrap();
            }
            let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
            // SAFETY: name is a terminated pathname owned by this test.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            let mut image = fs::read(env!("ELF_LOADER_LAYOUT_NONPIE")).unwrap();
            let phoff = u64::from_le_bytes(image[32..40].try_into().unwrap()) as usize;
            let phnum = u16::from_le_bytes(image[56..58].try_into().unwrap()) as usize;
            let index = (0..phnum)
                .find(|index| {
                    u32::from_le_bytes(
                        image[phoff + index * 56..phoff + index * 56 + 4]
                            .try_into()
                            .unwrap(),
                    ) == 3
                })
                .unwrap();
            let at = phoff + index * 56;
            let offset = image.len() as u64;
            let size = name.as_bytes_with_nul().len() as u64;
            image[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
            image[at + 32..at + 40].copy_from_slice(&size.to_le_bytes());
            image.extend_from_slice(name.as_bytes_with_nul());
            let path = scratch("fifo-interpreter-target.elf");
            fs::write(&path, image).unwrap();
            record_binary(
                &path,
                "layout-nonpie; append interpreter.fifo pathname and point PT_INTERP to it",
            );
            let target = File::open(path).unwrap();
            let (send, receive) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let result = prepare_start(
                    &target,
                    &Invocation::execve("./fixture").unwrap(),
                    Path::new("./h"),
                );
                let _ = send.send(result);
            });
            let result = receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("prepare_start blocked on a FIFO interpreter");
            worker.join().unwrap();
            assert!(matches!(
                result,
                Err(Error::UnsupportedElf("interpreter is not a regular file"))
            ));
            fs::remove_file(fifo).unwrap();
        },
    );
}

fn scratch(name: &str) -> PathBuf {
    Path::new(env!("ELF_LOADER_ARTIFACT_DIR")).join(name)
}

fn program_header_offsets(image: &[u8], kind: u32) -> Vec<usize> {
    let phoff = u64::from_le_bytes(image[32..40].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(image[56..58].try_into().unwrap()) as usize;
    (0..phnum)
        .map(|index| phoff + index * 56)
        .filter(|at| u32::from_le_bytes(image[*at..*at + 4].try_into().unwrap()) == kind)
        .collect()
}

fn write_preparation_fixture(name: &str, image: &[u8], operation: &str) -> File {
    let path = scratch(name);
    fs::write(&path, image).unwrap();
    record_binary(&path, operation);
    File::open(path).unwrap()
}

fn minimal_interpreter(kind: u16, address: u64, memsize: u64) -> Vec<u8> {
    let source = fs::read(env!("ELF_LOADER_LAYOUT_NONPIE")).unwrap();
    let mut image = source[..64].to_vec();
    image[16..18].copy_from_slice(&kind.to_le_bytes());
    // Keep the target's e_entry: Linux's zero-span refusal must not depend on
    // whether an interpreter entry points into an already mapped main image.
    image[32..40].copy_from_slice(&64_u64.to_le_bytes());
    image[40..48].fill(0);
    image[56..58].copy_from_slice(&1_u16.to_le_bytes());
    image[58..64].fill(0);
    image.resize(120, 0);
    let header = &mut image[64..];
    header[..4].copy_from_slice(&PT_LOAD.to_le_bytes());
    header[4..8].copy_from_slice(&4_u32.to_le_bytes());
    header[8..16].copy_from_slice(&(address % 4096).to_le_bytes());
    header[16..24].copy_from_slice(&address.to_le_bytes());
    header[24..32].copy_from_slice(&address.to_le_bytes());
    header[40..48].copy_from_slice(&memsize.to_le_bytes());
    header[48..56].copy_from_slice(&4096_u64.to_le_bytes());
    image
}

fn file_backed_interpreter(kind: u16, address: u64) -> Vec<u8> {
    assert_eq!(address % 4096, 0);
    let mut image = minimal_interpreter(kind, address, 4096);
    let size = image.len() as u64;
    image[96..104].copy_from_slice(&size.to_le_bytes());
    image
}

fn mixed_interpreter(kind: u16, loads: &[(u64, u64, u64)]) -> Vec<u8> {
    let mut image = minimal_interpreter(kind, 0, 0);
    image[56..58].copy_from_slice(&(loads.len() as u16).to_le_bytes());
    image.resize(4096, 0);
    for (index, &(address, filesize, memsize)) in loads.iter().enumerate() {
        let at = 64 + index * 56;
        image[at..at + 56].fill(0);
        image[at..at + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        image[at + 4..at + 8].copy_from_slice(&4_u32.to_le_bytes());
        for (field, value) in [
            (8, address % 4096),
            (16, address),
            (24, address),
            (32, filesize),
            (40, memsize),
            (48, 4096),
        ] {
            image[at + field..at + field + 8].copy_from_slice(&value.to_le_bytes());
        }
    }
    image
}

fn target_with_interpreter(name: &str, interpreter: &[u8], operation: &str) -> File {
    target_with_interpreter_from_main(
        name,
        interpreter,
        operation,
        fs::read(env!("ELF_LOADER_LAYOUT_NONPIE")).unwrap(),
    )
}

fn target_with_interpreter_from_main(
    name: &str,
    interpreter: &[u8],
    operation: &str,
    mut image: Vec<u8>,
) -> File {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let interpreter_path = scratch(&format!("{name}-interpreter.elf"));
    fs::write(&interpreter_path, interpreter).unwrap();
    record_binary(&interpreter_path, operation);
    let interpreter_name = CString::new(interpreter_path.as_os_str().as_bytes()).unwrap();
    let headers = program_header_offsets(&image, PT_INTERP);
    assert_eq!(headers.len(), 1);
    let at = headers[0];
    let offset = image.len() as u64;
    let size = interpreter_name.as_bytes_with_nul().len() as u64;
    image[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
    image[at + 32..at + 40].copy_from_slice(&size.to_le_bytes());
    image.extend_from_slice(interpreter_name.as_bytes_with_nul());
    write_preparation_fixture(
        &format!("{name}-target.elf"),
        &image,
        &format!(
            "provided main ELF; PT_INTERP points to {}",
            interpreter_path.display()
        ),
    )
}

fn record_binary(path: &Path, operation: &str) {
    let sha = Sha256::digest(fs::read(path).unwrap());
    let entry = format!(
        "generation: cargo --offline test -p reverie-elf-loader --test preparation; operation={operation}\nsha256: {sha:x}  {}\n",
        path.display()
    );
    let mut output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(scratch("preparation-images.provenance"))
        .unwrap();
    output.write_all(entry.as_bytes()).unwrap();
}
