/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Keep the loader below the admitted target address range. Mutable storage
 * is not part of any PT_LOAD: entry.S creates a shared anonymous mapping for
 * it before calling C. This adds no private writable pages to data_vm. */
OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

PHDRS
{
    code PT_LOAD FILEHDR PHDRS FLAGS(5);
    shadow PT_NOTE FLAGS(4);
    stack PT_GNU_STACK FLAGS(6);
}

SECTIONS
{
    . = 0x100000 + SIZEOF_HEADERS;
    .text : { *(.text .text.*) } :code
    .rodata : { *(.rodata .rodata.*) } :code
    /* The host replaces this PT_NOTE with a read-only PT_LOAD carrying the
     * target's start_data, end_data and start_brk geometry. */
    .note.placeholder : { LONG(0); LONG(0); LONG(0); LONG(0); } :code :shadow
    ASSERT(. < 0x200000, "loader text overlaps its scratch mapping")

    _scratch_start = 0x200000;
    . = _scratch_start;
    .record (NOLOAD) : { KEEP(*(.record)) } :NONE
    ASSERT(SIZEOF(.record) <= 4096, "handoff record exceeds one page")
    . = _scratch_start + 4096;
    _record_end = .;
    .bss (NOLOAD) : { *(.bss .bss.*) *(COMMON) } :NONE
    . = ALIGN(4096);
    _scratch_end = .;
    _scratch_size = _scratch_end - _scratch_start;
    _transient_size = _scratch_end - _record_end;
    ASSERT(_scratch_end <= 0x400000, "loader scratch exceeds reserved address band")

    /DISCARD/ : { *(.eh_frame .eh_frame.* .comment .note.GNU-stack .note.gnu.*) }
}
