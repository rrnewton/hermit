# Kernel start parity fixtures

`layout.c` links with libc, as PIE and as a fixed-address executable.
`layout_entry.S` replaces the ELF entry point and copies the original stack
to BSS without writing to that stack, then jumps to the ordinary CRT `_start`.
The copy covers original RSP through the end of the page containing the
AT_EXECFN terminating NUL, including
the entire auxv, stack padding, and the AT_RANDOM bytes. The dynamic linker has
already run at that entry, so the separate observer covers original registers
and the region below the kernel's entry RSP.

Layout output consists of `KEY=VALUE` lines. Addresses have a `0x` prefix,
counts and `/proc/self/stat` fields are decimal, and byte strings use lower
case hexadecimal without a prefix. `argv.N.hex`, `env.N.hex`, and
`execfn.hex` omit their terminating NUL; the raw stack and proc files retain
every byte. `auxc` includes the terminating AT_NULL pair. `program.base` is
the ELF load bias (zero for ET_EXEC). `maps` contains the entire proc file,
including VMA addresses, permissions, offsets, devices, inodes, and names.

The heap probe grows brk one page at a time, captures the first failed request
and its actual returned brk, then reduces the heap to one page so that there
is a `[heap]` mapping to compare. The default probe bound is 16 pages;
`ELF_LOADER_HEAP_PAGES=N` sets a bound from 1 through 4096. `data.baseline_kib`
and `data.peak_kib` are the fixture's observed VmData before and during this
probe. They do not describe the loader's earlier peak; the test runner must
observe that independently at syscall stops.

`observer.S` is a freestanding static PIE used as the dummy program's
PT_INTERP. It records general and extended registers before using CPUID or
modifying any SIMD register. RIP-relative stores save all GPRs first;
PUSHFQ runs on a separate BSS stack. FXSAVE on that private stack records the
x87 control/status words and abridged tag mask without changing them. Direct
segment-register reads record CS, SS, DS, ES, FS and GS; ARCH_GET_FS and
ARCH_GET_GS independently query the live bases. The observer never writes to
the original stack. Its binary stdout record is little endian:

| Offset | Size | Contents |
| --- | --- | --- |
| 0 | 8 | ASCII `ELFOBS01` |
| 8 | 8 | Header size, 2368 |
| 16 | 128 | u64 rax, rbx, rcx, rdx, rsi, rdi, rbp, rsp, r8 through r15 |
| 144 | 8 | RFLAGS |
| 152 | 4 | MXCSR |
| 156 | 2 | x87 control word |
| 158 | 2 | x87 status word, including TOP and condition codes |
| 160 | 8 | Feature mask: bit 0 YMM, bit 1 ZMM, bit 2 PKRU, bit 3 64-bit opmask |
| 168 | 8 | XCR0, zero if OSXSAVE is absent |
| 176 | 8 | Original RSP minus 1024, the snapshot start |
| 184 | 8 | Snapshot length |
| 192 | 256 | xmm0 through xmm15 |
| 448 | 256 | Upper 128 bits of ymm0 through ymm15, when supported |
| 704 | 512 | Upper 256 bits of zmm0 through zmm15, when supported |
| 1216 | 1024 | Full zmm16 through zmm31, when supported |
| 2240 | 64 | k0 through k7, each in a zero-padded u64 slot |
| 2304 | 4 | PKRU, when supported |
| 2308 | 1 | x87 abridged FTW: one bit for each nonempty physical register |
| 2310 | 12 | u16 CS, SS, DS, ES, FS and GS selectors |
| 2328 | 8 | FS base from ARCH_GET_FS |
| 2336 | 8 | GS base from ARCH_GET_GS |
| 2368 | variable | Original stack from RSP minus 1024 through the end of the AT_EXECFN page |

Reserved bytes and unsupported feature fields remain zero. AVX needs the
CPUID AVX/OSXSAVE bits and XCR0 SSE/AVX enablement. AVX512 needs AVX512F and
XCR0 SSE/AVX/opmask/ZMM enablement. Opmask reads use KMOVQ only with AVX512BW;
AVX512F alone uses KMOVW. PKRU needs both CPUID PKU and OSPKE.
Both native and loader records are compared with their respective ptrace
captures, then compared byte for byte with each other. These witnesses cover
the fields listed above; x87 data-register contents, instruction/data pointers
and additional XSAVE components are not asserted by this observer.

`observer_dummy.S` exits with status 99 if its own entry is reached, which
would mean the PT_INTERP observer did not take control. The observer exits 96
on a failed base query, 97 on a failed stdout write and 98 on an absent or
oversized initial stack.
The stack snapshot capacity is 256 KiB, comfortably above these fixtures'
fixed argv/envp; it is a fixture bound, not a loader admission constraint.

Example generation commands (the actual build records hashes and its exact
commands under `target/`):

```sh
cc -O1 -Wall -Wextra -Werror -fno-stack-protector -fPIE -pie \
  -Wl,-e,fixture_start layout.c layout_entry.S -o layout-pie
cc -O1 -Wall -Wextra -Werror -fno-stack-protector -fno-pie -no-pie \
  -Wl,-e,fixture_start layout.c layout_entry.S -o layout-nonpie
cc -nostdlib -static-pie -Wl,--build-id=none observer.S -o observer
cc -nostdlib -fPIE -pie -Wl,--build-id=none \
  -Wl,--dynamic-linker=/absolute/target/path/observer \
  observer_dummy.S -o entry-pie
cc -nostdlib -fPIE -pie -Wl,-Ttext-segment=0x400000 -Wl,--build-id=none \
  -Wl,--dynamic-linker=/absolute/target/path/observer \
  observer_dummy.S -o entry-nonpie
```

GNU ld's `-Ttext-segment=0x400000` emits ET_EXEC even with `-pie`. Using the
PIE link mode preserves PT_INTERP in a program without shared dependencies;
plain `-no-pie` would omit that interpreter segment.

## Execute-only protection probe

`pkey_probe.S` is a separate freestanding interpreter, compiled to probe either
the main's AT_ENTRY or its own entry (`-DPKEY_PROBE_INTERPRETER`). The tests
change the entry-containing PT_LOAD from RX (flags 5) to X (flags 1), requiring
that its file/memory sizes match and that no other load overlaps its pages.
Readable companions retain flags 5. The interpreter samples live PKRU before
installing a signal handler and performing an actual MOVB from the text.
It uses a private stack for both syscalls and the signal frame.

The handler accepts only SIGSEGV/SEGV_PKUERR with si_pkey=1, si_addr equal to
the requested byte and saved RIP equal to the deliberate MOVB. It resumes
after that instruction without changing the saved xstate; PKRU is sampled
again after rt_sigreturn. A successful read records the exact byte instead.
Malformed probes, unexpected faults, failed signal setup or failed output
exit 95. The 64-byte record is compared exactly, including zero padding:

| Offset | Size | Contents |
| --- | --- | --- |
| 0 | 8 | ASCII `ELFPKU01` |
| 8 | 4 | Live entry PKRU, independently matched to ptrace |
| 12 | 4 | PKRU after the read or rt_sigreturn, equal to entry |
| 16 | 8 | Requested text address |
| 24 | 8 | Readability: 1 for a successful read, 0 for a fault |
| 32 | 1 | Read byte, zero for a fault |
| 40 | 4 | Signal number, 11 for a fault |
| 44 | 4 | si_code, 4 (SEGV_PKUERR) for a fault |
| 48 | 4 | si_pkey, 1 for a fault |
| 56 | 8 | si_addr, equal to the requested address for a fault |

Main-only, interpreter-only and simultaneous execute-only mappings cover both
PIE and ET_EXEC mains. smaps witnesses independently require pkey 1 for X
text and pkey 0 for RX text. Loader-only pkey-1 permission seeding before its
first instruction makes the execute-only controls fail a stale restore even
when the host's default PKRU already denies access. Native PKRU is unchanged.
All ordinary entry comparisons still apply. Missing CPU PKU/OSPKE is the only
skip condition.
