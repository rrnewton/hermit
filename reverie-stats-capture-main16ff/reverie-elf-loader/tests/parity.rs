/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! These are ordinary Linux tests; no Hermit or Reverie backend is involved.
//! Both starts require ADDR_NO_RANDOMIZE. At the ptrace exec stop we replace
//! the 16 AT_RANDOM bytes with the same witness bytes before any instruction
//! executes. Nothing else in the compared stack is normalized.

mod support;

use support::Fixture;
use support::Mutation;
use support::Naming;
use support::TestResult;

macro_rules! layout_test {
    ($name:ident, $fixture:ident, $naming:ident) => {
        #[test]
        fn $name() {
            support::test(stringify!($name), || {
                support::layout_case(stringify!($name), Fixture::$fixture, Naming::$naming, None)
            });
        }
    };
}

layout_test!(pie_absolute, Pie, Absolute);
layout_test!(nonpie_absolute, NonPie, Absolute);
layout_test!(pie_relative, Pie, Relative);
layout_test!(nonpie_relative, NonPie, Relative);
layout_test!(pie_directory_descriptor, Pie, DirectoryDescriptor);
layout_test!(nonpie_directory_descriptor, NonPie, DirectoryDescriptor);
layout_test!(pie_empty_path_descriptor, Pie, EmptyPath);
layout_test!(nonpie_empty_path_descriptor, NonPie, EmptyPath);
layout_test!(pie_deleted_file, Pie, Deleted);
layout_test!(nonpie_deleted_file, NonPie, Deleted);
layout_test!(pie_memfd, Pie, Memfd);
layout_test!(nonpie_memfd, NonPie, Memfd);

#[test]
fn bin_true() {
    support::test("bin_true", || {
        support::layout_case("bin_true", Fixture::True, Naming::Absolute, None)
    });
}

#[test]
fn observed_initial_register_and_segment_state() {
    support::test("observed_initial_register_and_segment_state", || {
        for fixture in [Fixture::EntryPie, Fixture::EntryNonPie] {
            support::observer_case(
                "observed_initial_register_and_segment_state",
                fixture,
                Mutation::None,
            )?;
        }
        Ok(())
    });
}

#[test]
fn execute_only_main_text_preserves_pkru() {
    support::test("execute_only_main_text_preserves_pkru", || {
        support::pkey_text_case("execute_only_main_text_preserves_pkru", true, false, false)
    });
}

#[test]
fn execute_only_interpreter_text_preserves_pkru() {
    support::test("execute_only_interpreter_text_preserves_pkru", || {
        support::pkey_text_case(
            "execute_only_interpreter_text_preserves_pkru",
            false,
            true,
            true,
        )
    });
}

#[test]
fn execute_only_main_and_interpreter_text_preserve_pkru() {
    support::test(
        "execute_only_main_and_interpreter_text_preserve_pkru",
        || {
            for probe_interpreter in [false, true] {
                support::pkey_text_case(
                    "execute_only_main_and_interpreter_text_preserve_pkru",
                    true,
                    true,
                    probe_interpreter,
                )?;
            }
            Ok(())
        },
    );
}

#[test]
fn readable_text_pkru_controls() {
    support::test("readable_text_pkru_controls", || {
        for probe_interpreter in [false, true] {
            support::pkey_text_case(
                "readable_text_pkru_controls",
                false,
                false,
                probe_interpreter,
            )?;
        }
        Ok(())
    });
}

#[test]
fn finite_data_limit() {
    support::test("finite_data_limit", || {
        for fixture in [Fixture::Pie, Fixture::NonPie] {
            // This reaches a real brk failure well before the 1024-page probe
            // cap. The comparator requires the failure request and result to
            // match, and observes VmData after every traced syscall.
            support::layout_case(
                "finite_data_limit",
                fixture,
                Naming::Absolute,
                Some(2 * 1024 * 1024),
            )?;
        }
        Ok(())
    });
}

#[test]
fn finite_data_startup_admission_boundary() {
    support::test("finite_data_startup_admission_boundary", || {
        support::finite_startup_boundary()
    });
}

#[test]
fn path_max_relative_and_directory_descriptor() {
    support::test("path_max_relative_and_directory_descriptor", || {
        support::path_boundary()
    });
}

#[test]
fn mutation_controls() {
    support::test("mutation_controls", || -> TestResult {
        for (mutation, assertion) in [
            (Mutation::OmitPlaceholder, "heap_metadata"),
            (Mutation::SkipVdsoRelocation, "mapping_parity"),
            (Mutation::DirtyRegisters, "entry_registers"),
            (Mutation::DirtyX87Status, "entry_registers"),
            (Mutation::DirtyX87Tag, "entry_registers"),
            (Mutation::DirtySelectors, "entry_registers"),
            (Mutation::DirtySegmentBases, "entry_registers"),
        ] {
            support::mutation_case(mutation, assertion)?;
        }
        Ok(())
    });
}

#[test]
fn et_exec_main_preserves_et_dyn_interpreter_hint() {
    support::test("et_exec_main_preserves_et_dyn_interpreter_hint", || {
        support::interpreter_address_hint(
            "et_exec_main_preserves_et_dyn_interpreter_hint",
            Fixture::EntryNonPie,
        )
    });
}

#[test]
fn pie_main_discards_et_dyn_interpreter_hint() {
    support::test("pie_main_discards_et_dyn_interpreter_hint", || {
        support::interpreter_address_hint(
            "pie_main_discards_et_dyn_interpreter_hint",
            Fixture::EntryPie,
        )
    });
}

#[test]
fn pie_zero_alignment_unaligned_first_load() {
    support::test("pie_zero_alignment_unaligned_first_load", || {
        support::unaligned_pie_alignment(
            "pie_zero_alignment_unaligned_first_load",
            None,
            0x555555554000,
        )
    });
}

#[test]
fn pie_mixed_alignment_unaligned_first_load() {
    support::test("pie_mixed_alignment_unaligned_first_load", || {
        // maximum_alignment rounds a nonzero maximum to a page, including
        // p_align=1 and 2, while preserving a maximum of zero.
        for (alignment, expected_bias) in [
            (1, 0x555555553000),
            (2, 0x555555553000),
            (4096, 0x555555553000),
            (0x200000, 0x5555553ff000),
        ] {
            support::unaligned_pie_alignment(
                &format!("pie_mixed_alignment_unaligned_first_load-{alignment:x}"),
                Some(alignment),
                expected_bias,
            )?;
        }
        Ok(())
    });
}

#[test]
fn last_header_mapping_matches_native_at_phdr() {
    support::test("last_header_mapping_matches_native_at_phdr", || {
        support::duplicate_header_mapping()
    });
}

#[test]
fn interpreter_load_span_zero_and_positive_controls() {
    support::test("interpreter_load_span_zero_and_positive_controls", || {
        support::interpreter_load_span()
    });
}

#[test]
fn interpreter_hint_reserved_range() {
    support::test("interpreter_hint_reserved_range", || {
        support::interpreter_hint_reserved_range()
    });
}

#[test]
fn interpreter_hint_exceptions() {
    support::test("interpreter_hint_exceptions", || {
        support::interpreter_hint_exceptions()
    });
}

#[test]
fn interpreter_mixed_empty_reserved_range() {
    support::test("interpreter_mixed_empty_reserved_range", || {
        support::interpreter_mixed_empty_reserved_range()
    });
}

#[test]
fn interpreter_empty_bss_reserved_range() {
    support::test("interpreter_empty_bss_reserved_range", || {
        support::interpreter_empty_bss_reserved_range()
    });
}

#[test]
fn interpreter_empty_negative_bias_reserved_range() {
    support::test("interpreter_empty_negative_bias_reserved_range", || {
        support::interpreter_empty_negative_bias_reserved_range()
    });
}

#[test]
fn main_reserved_range() {
    support::test("main_reserved_range", support::main_reserved_range);
}

#[test]
fn mdwe_executable_bss_controls() {
    support::test("mdwe_executable_bss_controls", || {
        support::mdwe_executable_bss_controls()
    });
}

#[test]
fn mdwe_supported_bss_controls() {
    support::test("mdwe_supported_bss_controls", || {
        support::mdwe_supported_bss_controls()
    });
}

#[test]
fn privileged_executable_mode_controls() {
    support::test("privileged_executable_mode_controls", || {
        support::privileged_executable_mode_controls()
    });
}

#[test]
fn privileged_executable_capability_controls() {
    support::test("privileged_executable_capability_controls", || {
        support::privileged_executable_capability_controls()
    });
}

#[test]
fn kernel_bss_right_merge_controls() {
    support::test("kernel_bss_right_merge_controls", || {
        support::kernel_bss_right_merge_controls()
    });
}

#[test]
fn kernel_initial_stack_overlap_controls() {
    support::test("kernel_initial_stack_overlap_controls", || {
        support::kernel_initial_stack_overlap_controls()
    });
}

#[test]
fn kernel_reserved_top_page_controls() {
    support::test("kernel_reserved_top_page_controls", || {
        support::kernel_reserved_top_page_controls()
    });
}

#[test]
fn kernel_interpreter_entry_controls() {
    support::test("kernel_interpreter_entry_controls", || {
        support::kernel_interpreter_entry_controls()
    });
}

#[test]
fn hugetlb_inode_controls() {
    support::test("hugetlb_inode_controls", || {
        support::hugetlb_inode_controls()
    });
}

#[test]
fn kernel_mapping_size_controls() {
    support::test("kernel_mapping_size_controls", || {
        support::kernel_mapping_size_controls()
    });
}

#[test]
fn empty_last_load_zero_data_controls() {
    support::test("empty_last_load_zero_data_controls", || {
        support::empty_last_load_zero_data_controls()
    });
}
