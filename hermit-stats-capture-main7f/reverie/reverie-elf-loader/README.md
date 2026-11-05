# reverie-elf-loader

A standalone building block for starting an ELF image inside its new process.
It is **unwired**: no Reverie backend or Hermit runtime calls it. It implements
LA ELF start preparation and the inactive LB precommit exec building blocks.
The in-guest production exec gate continues to refuse exec. LB includes an
actual runtime clock restoration fixture; host activation remains unqualified.
Runtime takeover and the dynamic manifest consumer belong to later stages.

## Inactive exec preparation (LB)

`unsafe prepare_exec(&ExecRequest, &PrepareExecOptions)` accepts the original
`dirfd`, literal path, argv, envp and flags. It returns
`ExecCheckOutcome::{NativeErrno, Refuse, Prepared}`. Native errors include their
source stage. Refusals expose stable names through `ExecRefusal::name()`.
`prepare_exec_raw` also accepts invalid user addresses. It qualifies the
filename before sending the original pointers to CHECK, then copies argument
vectors after success. A failed remote filename copy requires a kernel
user-copy certificate through invalid-FD `fgetxattr`, which cannot traverse a
pathname; otherwise preparation refuses. Owned requests use stable Rust buffers.

The caller must use an isolated ordinary preparation process with exclusive
descriptor-table allocation and a genuinely frozen namespace, credential,
security, integrity, binfmt, watch, resource and executable-content contract.
`HostQualification::collect_current()` refuses before opening an endpoint.
`collect_current_from` reads only retained, already-qualified genuine proc
descriptors. Their PID, mount and user namespaces, task identity and lifetime
must match the complete frozen mount snapshot. Collection still cannot prove
security/binfmt/watch lifetimes. The unsafe external-attestation constructor
documents its required provenance and lifetime. `EvidenceOrigin::Modeled` is reserved for
declared inactive tests. Model receipts do not qualify a host for activation.
The real launcher caller must separately prove the preserved thread-group
leader, signal-context, installed SIGALRM handler and foreign-seccomp exclusions.
Modeled preparation on ordinary libtest workers does not qualify those contexts.

Preparation establishes capacity for 16 private descriptors before target
lookup. It releases its temporary placeholders for lookup qualification and
the original path CHECK, then reacquires the same numbers before ordinary
pinning. This preserves originally
invalid low dirfds and absent procfd names; exclusive allocation makes the
reacquisition deterministic. Native exec can work with a full user FD table,
so private exhaustion is `LauncherFdCapacity`. Guest descriptors 100, 102 and
numbers above 1024 retain their numbers, contents, OFDs and flags.

Absolute lookup uses retained admitted mount roots; relative interpreter
lookup qualifies the current CWD before each script rewrite and PT_INTERP
lookup. A constrained O_PATH `openat2` probe uses NO_XDEV and NO_MAGICLINKS
before original CHECK or interpreter pinning, preventing aliases from walking
generic proc objects or referring to new private descriptors. The original
probe is closed before CHECK. Exact retained leaf mount roots need no probe.
Unverifiable mount crossings or magic-link aliases receive named refusals.
Before a failed mount-relative interpreter probe can supply a native errno,
preparation proves the original absolute prefixes in order. A successful
original CHECK supplies search evidence for its shared absolute prefix;
remaining mount boundaries require constrained parent `/.` lookups. This
preserves ancestor EACCES before a missing interpreter's ENOENT. An unqualified
prefix yields a named refusal. Successful probes retain the original-path pin.

Targets, script interpreters and PT_INTERP objects are pinned with O_PATH and
classified before any readable reopen. Readable files come from the pinned
retained genuine proc FD directory and must match device, inode, mount ID,
size and timestamps. No absolute proc pathname is opened by preparation.
Execute-authorized unreadable objects get `ExecutableReadRequired`. Anonymous
memfds require a receipt binding the exact inode and immutable seals, followed
by seal verification on the readable descriptor. Generic interpreter lookup
through proc/sys/dev gets `LookupMountUnverified`; individually audited helper
endpoints permit original genuine `/proc/self/exe` and guest `/proc/self/fd/N`
requests, with namespace evidence and lookup-base qualification. Helper-owned
retained descriptor numbers and generic aliases are refused. These exceptions
do not authorize guest interpreter names or private FD aliases.
Complete mount qualification remains required before other traversal.

`AT_EXECVE_CHECK` supplies executable-open authorization and original argument
errors. Preparation separately ports the x86-64 precommit ELF and script
checks. CHECK success alone cannot establish ELF acceptance. Extra pinned
target denials get `PinnedAuthorizationChanged`; extra interpreter policy
denials get `InterpreterCheckStronger` unless the native denial source is
proved. Unexpected extra read/resource errors remain refusals. Existing LA
geometry and resource restrictions keep their original named refusals.

`ArgumentPlan` retains the original argc/envc pointer allowance, original F,
empty argv normalization, optional argument and nested script rewrites. Native
E2BIG remains E2BIG; the recomputed rewritten launcher band may instead refuse
`LauncherArgumentBudget`. `ArgumentPages` independently replays argument VMA
growth against RLIMIT_STACK, including the retained high-water pages after
removing argv0. A rewrite that needs a new page can return native E2BIG after
the original CHECK fits, before opening the interpreter. CHECK E2BIG carries its classification, including
the observed case where both size models pass and kernel stack growth fails.

`PinnedStart` owns final ELF T, pinned interpreter I, script pins, rewritten
arguments, original invocation, LA start plan and preparation evidence.
`prepare_start_from_files` supports caller-proved T/I bindings without resolving
the interpreter pathname. `transfer_files` and descriptor transactions support
explicit CLOEXEC transfer/rollback; undo failures are reported. The bounded,
versioned `StartManifest` describes the future private descriptor protocol.
The existing freestanding consumer still uses its LA protocol below.

The proc building blocks use actual pinned proc identities and sealed auxv
OFD snapshots. Dup shares the kernel cursor; old descriptors retain their old
snapshot. Later exec with inherited virtual state currently gets
`InheritedVirtualProcStateUnsupported` before commit. `ClockStateCarrier` is a
sealed memfd carrying a nonzero runtime RCB snapshot and opaque owner bytes.
The LB7 native successor fixture restores that count under a changed raw
counter origin and reads through Reverie's actual in-guest counter reader.
Omitting the serialized offset must fail the same trajectory comparator.
Small unused snapshot/restore/counter-binding APIs live in `reverie-inguest`;
the fixture controls perf metadata and requires no host PMU. The fixture Tool's
nonzero logical time and committed RCB boundary are restored into ToolHost's
actual ThreadState, then read and advanced through dispatch and Guest access.
Omitting only logical time fails the same owner trajectory assertion. The
carrier keeps those bytes opaque, and no production exec consumes it. See
[CLOCK-STATE.md](CLOCK-STATE.md) for the fixture's scope and owner read path.

LB tests start bounded ordinary native/preparation children. Fixtures, trace
logs and evidence stay under `target/`. Target exec, CHECK and CWD setup run
after spawn in a fresh helper under its parent's timeout; `pre_exec` only
changes inherited descriptor flags. Timeout failure is independent of reaping:
cleanup polls asynchronously for at most 250 ms. Atomic directory creation
gives each invocation its own fixtures. Public libc open/statx instrumentation
has live positive controls and read-open/statx-first mutations. Mandatory
syscall tracing separately observes the direct constrained lookup probes,
including guard-omission/flag/readable-open mutations; kernel-internal CHECK
ordering remains source audited. Genuine proc fixtures retain complete,
unfiltered private mount namespaces and compare CHECK/preparation with bounded
actual exec in that same namespace. Security/binfmt/unsafe-filesystem
and privileged-context fixtures are explicitly modeled. Privileged policy,
namespace-init, noexec/idmapped/watch qualification remains an activation gate.
The production inactivity test calls the real in-guest dispatcher and requires
`-EOPNOTSUPP` for execve and execveat, including CHECK.

## Build

`build.rs` invokes the system C compiler to build `loader/loader.c` and
`loader/entry.S` with `loader/loader.ld`. The result is a static, freestanding
ET_EXEC at `0x100000`: no libc, dynamic loader, allocator, or Rust runtime.
The Rust host library embeds that template and constructs each sparse start
image. C and assembly retain the proven syscall/mapping implementation from
the supplied `lp.c`; assembly gives explicit control over the first and last
instructions. No extra Cargo build dependency or network fetch is needed.

The build also generates PIE and non-PIE observation fixtures, an interpreter
entry observer, and a separate loader with test mutations enabled. The
production template rejects nonzero mutation flags. Every generation command,
source hash, binary SHA-256 and compiler version is recorded in
`target/debug/build/reverie-elf-loader-*/out/PROVENANCE.txt`. Compiler logs,
generated images, and test evidence stay under `target/`.

This workspace centralizes Buck rules in the root `BUCK`, so the new crate's
`rust_library`, freestanding executable `genrule`, and host-policy control
`genrule` are there too. The Buck unit-test environment binds both the loader
template and the control executable; Cargo uses `build.rs`. The ordinary
Cargo parity suite is independent of Meta's Buck test infrastructure.

## Start protocol

1. Pin a readable regular ELF file. Construct `Invocation::execve` or
   `Invocation::execveat` with the literal filename and child descriptor number.
   Preparation uses the kernel's execfn rules: a relative dirfd call synthesizes
   `/dev/fd/N/path`; an empty-path call synthesizes `/dev/fd/N`.
2. Call `prepare_start`, naming a short symlink that the caller will create.
   Redundant separators pad the symlink filename to the native execfn length.
   Execute that padded name with exactly the target's argv and envp.
3. `PreparedStart::write_image` writes the sparse per-start ELF. Its replaceable
   PT_NOTE becomes an R-only PT_LOAD with the target's native `start_data`,
   `end_data`, and raw memory end. The kernel itself installs those mm fields
   and page-rounds the memory end to obtain `start_brk`.
4. In the child, disable ASLR with `personality(ADDR_NO_RANDOMIZE)`. Pass the
   target on fd 100 and `PreparedStart::metadata()` on fd 102, both without
   CLOEXEC. Metadata is `execfn\0comm\0flags`; the production flags byte is zero.
   Keep other caller descriptor state identical to the native control.
5. The loader uses its own shared scratch mapping, leaving the original stack
   untouched while it reads the target/interpreter. It removes the placeholder,
   maps both ELF images, and relocates vvar/vvar_vclock/vdso to their native
   positions. It patches scalar auxv types 3, 5, 7, 9, and 33, plus the bytes
   addressed by type 31 (`AT_EXECFN`). The host plan lists these patches.
6. The loader sets native comm, closes its target/metadata/interpreter FDs,
   removes transient scratch, makes the final record read-only, resets extended
   processor state while preserving the current PKRU, and jumps to the
   interpreter with zero GPRs except RSP.

The caller owns argv/envp preparation, descriptor reservation, immutable file
identity and exec authorization. It must arrange direct `binfmt_elf` execution
and the same resulting credentials and secureexec state for target and launcher,
including any inode-sensitive LSM policy. A binfmt_misc recursive handoff is
outside this protocol. Preparing a start does not execute anything.
The loader diagnoses its own post-exec failures on stderr and exits 127; it
does not implement the later backend's failure or rollback protocol.

## Admitted scope and named refusals

The implementation supports little-endian ELF64 x86-64 ET_EXEC and ET_DYN
programs with PT_INTERP and exactly one non-executable PT_GNU_STACK. Headers
must have ordinary page-congruent load geometry, ascending load addresses,
and at most 128 program headers. ELF32, static targets, executable/missing or
repeated stack headers, malformed/unsupported ELF geometry, launcher paths
longer than the native filename, and unsupported execveat flags have explicit
errors. Raw and biased load ranges must end at or below `0x7ffffffff000`, the
four-level x86-64 TASK_SIZE and default mmap window. `Error::ReservedTopPage`
refuses the page starting there and addresses above it. This is a conservative
loader scope bound on five-level hosts, whose actual TASK_SIZE is larger.

`Error::MappingTooLarge` caps every nonempty page-rounded load extent and every
attempted first file reservation at 16 GiB. With at most 128 headers in each
image, this leaves a large free gap above the loader band below even the lowest
native mmap base. The interpreter and vDSO allocator therefore cannot select a
different fallback solely because of retained loader mappings. Empty first
loads do not reserve their total span. Main and interpreter controls qualify
the exact 16 GiB boundary with full parity and execute native-valid loads one
page above it before requiring the named preparation and loader refusals.

`Error::StackGuardGapOverride` requires the kernel's default 1 MiB stack guard.
Preparation and loading refuse any visible `stack_guard_gap=` boot override,
including small or invalid values. The default guard plus the top 8 MiB stack
bound stays above the minimum 128 MiB mmap gap, so constructing tables earlier
cannot move the allocator's search ceiling. A mandatory control passes injected
boot strings through the actual Rust guard and a C build calling the loader's
same guard and diagnostic; it changes no boot setting or production mutation
flag. Readable `/proc/cmdline` is part of the host protocol.

Preparation reads `/proc/sys/vm/mmap_min_addr`; the loader reads it again before
moving vvar/vDSO. Their temporary base is the larger of `0x20000` and the
page-rounded minimum. `Error::MmapMinAddrTooHigh` refuses minima above `0x80000`,
conservatively leaving room for the largest supported `0x80000` special-map
span below loader text at `0x100000`. The loader checks that this whole band is
free in `/proc/self/maps`; admitted main/interpreter mappings start at or above
`0x400000`. Pure Rust/C controls qualify minima through `0x80000`, including
`0x40000`, and require the exact named refusal above it and for overflow-sized
inputs. They change no host setting. Readable procfs sysctls are required.
This selection respects the exposed DAC minimum; stricter LSM mapping policy
can still reject the move through the existing runtime diagnostic.

`Error::HugetlbElf` refuses hugetlb-backed programs and interpreters by checking
the pinned file's filesystem type. Kernel ELF mapping calls `vm_mmap` directly;
syscall `mmap` first rounds hugetlb file lengths to the huge-page size. Controls
verify real hugetlb inodes, a 4 KiB syscall request producing a 2 MiB VMA without
faulting pages, and both named refusals. They also require ordinary full parity
before and after. The host has no allocated huge-page pool, so a native-valid
hugetlb ELF is source-audited rather than executed by this control.

PIE placement preserves Linux's zero maximum alignment: when every PT_LOAD has
`p_align=0`, the PIE base is not masked before subtracting the first raw vaddr.
An unaligned first `p_offset=p_vaddr=0x123` therefore gets load bias
`0x555555554000`. A nonzero maximum alignment is page-rounded and masks the
base; mixed zero/1/2/4096/2 MiB alignment controls check those separate formulas.

`Error::PrivilegedExecutable` refuses a target with either setid mode bit or
any `security.capability` xattr, including an empty capability set. Such execs
can clear ADDR_NO_RANDOMIZE even without changing UID/GID, and can also change
credentials, AT_SECURE and the stack limit. The loader repeats the check on the
pinned target. The interpreter inode does not determine exec credentials.
Controls qualify ordinary starts, execute native setid and empty-capability
starts, require both named refusals, then restore the same inode and full parity.

`Error::ZeroInterpreterLoadSpan` refuses an interpreter whose maximum
`PT_LOAD` memory end minus its minimum page-aligned load address is zero,
matching Linux 6.17. The loader repeats this check defensively. Tests require
native SIGSEGV and the two named refusals for zero spans, and native/loader
parity for a one-byte positive span, with both ET_EXEC and ET_DYN interpreters.

An ET_DYN interpreter's first load address remains an mmap hint when the main
image has zero load bias. A biased main discards that hint, as Linux does.
`Error::InterpreterHintOverlapsLoader` refuses a preserved, nonzero
page-aligned file-mapping hint below `0x400000`: its initial span intersects the reserved
lower address band used by loader code, scratch and temporary special mappings.
The loader repeats this check defensively. Ordinary file-backed zero hints and
file-mapping hints discarded by a biased main remain supported. Controls use a free `0x800000`
interpreter address with ET_EXEC and PIE mains, and independently free spans
at `0x3ff000` (refused) and `0x400000` (accepted), with the main placed higher.
Preparation and loading both use the last PT_LOAD containing the program
header table for AT_PHDR; controls map the header page twice.

`Error::PaddedPathTooLong` includes the terminating NUL in its reported size.
4095 filename bytes qualify; 4096 do not. A 4095-byte relative path can succeed
in native execveat while its `/dev/fd/N/` prefix requires a named loader refusal.
The suite tests the real native call at this boundary.

`Error::LowLoadSegment` refuses resolved program segments and every nonempty
interpreter mapping range below `0x400000`, including file pages, partial-page
zeroing and anonymous BSS. PIE addresses are checked after applying their native
bias. An empty interpreter load maps nothing, even at an unaligned address, but
still establishes the bias. In particular, a first load with zero file size
performs no initial mmap: its bias is zero for an unbiased main and minus its
page-aligned address for a biased main. Preparation checks every later effective
load in both cases. A file-backed first dynamic load reserves the whole span
through nonfixed mmap; preparation checks every relative range and the loader
checks the actual reservation before releasing its tail or touching BSS. The
loader also checks every fixed file and anonymous mapping before replacing it.

`Error::InterpreterEntry` checks the resolved unsigned interpreter entry,
including a negative ET_DYN bias, against the admitted address window. A
file-backed ET_DYN interpreter's actual mmap-selected bias is checked by the
loader; preparation conservatively refuses raw entries above the same window.
The entry need not belong to an interpreter PT_LOAD: an interpreter may enter
the mapped main program, as the positive one-byte-span controls demonstrate.

`Error::InitialStackOverlap` refuses nonempty loads, whole first reservations
and shadow mappings intersecting `[0x7fffff7ff000, 0x7ffffffff000)`. Linux maps
the ELF before building initial tables; this loader reuses tables built by its
own exec. The top 8 MiB covers the maximum 6 MiB string/pointer budget, 128 KiB
stack expansion, and remaining page/table rounding for every admitted argv,
envp and stack limit with ASLR off. Empty loads map nothing and remain supported
inside this band; their raw metadata and rounded brk must still match exactly.

`Error::BssRightMerge` refuses an anonymous BSS mapping that would join a
surviving compatible BSS VMA on its right. Linux's `vm_brk_flags` considers only
the preceding VMA; anonymous mmap can merge both sides. Preparation and loading
track replacements and full reservations across both images. Main-only,
interpreter-only and cross-image controls independently qualify the native
two-VMA shape, require the named refusal, and retain ordinary parity for a
companion with different execute permission on the right.

Native controls qualify mixed empty/nonempty and pure-BSS interpreters with both
ET_EXEC and PIE mains: `0x3ff000` and `0x100000` get named preparation and
defensive loader refusals; `0x400000` passes exact entry and assembly-observer
parity. A PIE control with an empty first load at `0x800123` checks the same
boundaries after its `-0x800000` interpreter bias. A separate ET_EXEC main control
qualifies `0x400000` and refuses `0x3ff000` in both preparation and loading.

The derived shadow mapping and heap start stay above the same bound. Image
generation repeats the check for a caller-modified public `ProgramLayout` before
writing the image. Linker assertions keep loader code and scratch within their
reserved band. Temporary vvar/vDSO moves are bounded below loader code; their
final mmap-selected destination is checked against the band and user limit
before any fixed remap.

The supported host supplies readable procfs maps and an enabled, unsealed x64
vDSO with the usual contiguous vvar/vvar_vclock/vdso layout. Missing, malformed,
oversized or sealed special mappings produce named runtime diagnostics. The
loader moves the original kernel VMAs, preserving their protections, backing
objects and namespace-dependent pages; it does not copy their contents.

`ProgramLayout::memory_end` retains the raw maximum biased PT_LOAD memory end.
The shadow uses `memsz=memory_end-start_data`, rather than a rounded brk extent.
An unaligned, empty final load therefore gives a zero-sized shadow and causes
no unmap during loading. RX-only controls pass with hard RLIMIT_DATA=0, exact
zero VmData peak, and native brk rounded to the next page; a real one-byte BSS
companion still needs exactly one page of data budget.

`Error::MdweExecutableBss` refuses executable anonymous BSS pages in either
the program or interpreter under inherited `PR_MDWE_REFUSE_EXEC_GAIN` (MDWE).
Linux 6.17 creates these pages with `vm_brk_flags`, which permits this native
exec case; the loader's anonymous RWX `mmap` is rejected by MDWE. This is a
loader scope restriction, not a native execution error. Non-executable BSS,
executable BSS confined to the last file-backed page, and MDWE-off starts remain
supported. Preparation reads the calling process's inheritable MDWE setting;
callers that enable MDWE later must repeat preparation in a process context
safe for Rust allocation and I/O, such as a separately exec'd preparation helper.
`PR_MDWE_NO_INHERIT` clears MDWE at exec and does not trigger this refusal.
The loader also checks its actual post-exec MDWE state and gives a named
diagnostic if this restriction is bypassed. Controls set MDWE in the child
before exec; they skip with a clear message only when the kernel lacks
`PR_SET_MDWE`.

Execute-only text PT_LOADs (`PF_X` without `PF_R` or `PF_W`) remain supported
in both the program and interpreter. With PKU/OSPKE, Linux assigns these
mappings an execute-only pkey and updates PKRU to deny data access. Entry
XRSTOR excludes component 9 (PKRU), preserving the value after all mappings
without a temporary reset or a stale pre-mapping restore. The later scratch
munmap and mprotect(PROT_READ) do not change that pkey state. The Linux 6.17
source check found no analogous mapping-created state in the other reset
components: ordinary FP/vector/MPX/APX state stays at its exec initial value,
AMX permissions require an explicit request, and CET/PASID/LBR/processor trace
are supervisor components outside the XCR0 reset mask.

Finite `RLIMIT_AS` has a named refusal: the extra mappings consume address
space. Finite `RLIMIT_DATA` is supported at or above the target/interpreter ELF
startup footprint. `Error::FiniteDataLimitBelowStartupFootprint` refuses smaller
limits, reporting both the limit and its conservative page accounting boundary.
Both no-libc entry fixtures succeed at exactly **282624 bytes (69 pages)**;
native execution fails with SIGSEGV one byte below, and preparation gives the
named refusal there. `/bin/true` has a 24576-byte ELF startup boundary.

Shared scratch is excluded from Linux's private writable data accounting.
Although shadow file pages are read-only, its anonymous BSS is writable and
counts; the shadow and real data mappings replace each other. Tests sample
VmData at every traced syscall and require loader peak accounting to be no
larger than the native control, alongside an identical first failed brk request
at a finite 2 MiB limit. Overlapping loads can make the admission budget
conservative; those refused starts are not reported as native execution errors.

For AT_EMPTY_PATH, comm comes from the executable dentry. A procfd link ending
in ` (deleted)` with nonzero link count is ambiguous: it might be a live name
with that literal suffix or an unlinked dentry with surviving hardlinks.
`Error::AmbiguousDescriptorComm` refuses this class. Fully deleted files and
memfds have zero link count and are covered by the parity tests. Preparation
classifies the interpreter through O_PATH before reopening the pinned regular
file, so a malformed PT_INTERP FIFO cannot block an ordinary read-open.

## Evidence and exact differences

The ordinary Linux integration tests run kernel and loader starts with ASLR
disabled. At the exec stop, ptrace supplies identical 16-byte `AT_RANDOM`
witnesses to both runs before any instruction. This is the only replacement
of inherently random data; comparisons keep stack addresses and all other
stack bytes exact. A restored instruction breakpoint captures both starts at
the interpreter's actual first instruction, avoiding syscall-stop register
artifacts. Procfs loader values are bound to their original kernel exec event.
The `observed_initial_register_and_segment_state` test checks GPRs/RIP,
RFLAGS, MXCSR, x87 FCW/FSW and abridged FTW, all six segment selectors, FS/GS
bases, XMM, and supported YMM/ZMM/opmask/PKRU fields exactly. The independent
assembly observer stores GPRs before changing registers, switches to its own
stack before PUSHFQ and FXSAVE, and reads segment selectors directly and bases
with ARCH_GET_FS/GS. It samples the original stack through the top page.
Each native and loader observation must agree with its own ptrace capture;
the two records must also agree byte for byte. CPUID and XCR0 gate
YMM, ZMM/opmask and PKRU observations. The test does not assert x87 data
registers, instruction/data pointers or every possible XSAVE component.

Execute-only controls cover main text, interpreter text and both together,
with PIE and ET_EXEC mains. A separate assembly interpreter samples live
PKRU and attempts a byte read from the selected text. Its signal handler
requires SIGSEGV/SEGV_PKUERR, the exact probe address/instruction and pkey 1;
rt_sigreturn leaves the saved xstate unchanged, and a second RDPKRU must match
entry. smaps independently requires ProtectionKey=1 and execute-only VMA
permissions. Readable RX companions must read the exact ELF byte through
pkey 0. All ordinary entry/stack/maps/register checks remain in force.

For discrimination with restrictive default PKRU, only the loader child has
pkey 1's AD/WD pair cleared at its actual first instruction, before any loader
mapping. A restored breakpoint and GET/SETREGSET readback bind that seed to
the live exec state; native execution is untouched. The mapping must restore
the pair to AD=1/WD=0 and produce exact native PKRU and readability parity.
These controls skip with a clear message only if CPUID lacks PKU or OSPKE;
missing advertised xstate, ptrace or protection evidence is a failure.

The allowed procfs differences are an **exact set**, not a filter for arbitrary
new differences:

| Surface | Expected difference |
| --- | --- |
| `/proc/self/exe` | Prepared loader image rather than target file |
| `/proc/self/auxv` | Exactly value types **3, 5, 7, 9, 33** retain the kernel's loader values; separate low-interpreter-hint and empty-first controls require `AT_BASE` to be their independently known bias and native vDSO to equal loader kernel-origin vDSO, with exactly **3, 5, 9** differing for zero bias and **3, 5, 7, 9** for the PIE negative-bias control |
| `/proc/self/stat` | Code fields **26 and 27** retain loader code bounds |
| `/proc/self/maps` | Loader RX file mapping at `0x100000`, and exactly one `0x200000–0x201000` R-only shared record mapping |

All stack auxv entries (including AT_NULL), argv, envp, execfn, initial stack
pointer/content, native program/interpreter/libc bases, special mappings,
heap/data fields, comm and cmdline must agree. Map comparisons retain ordinary
rows exactly and validate each extra loader row's extent and permissions.

Mutation controls execute seven faulty starts and require the ordinary
comparator to fail by name: omitted placeholder → `heap_metadata`; omitted
vDSO relocation → `mapping_parity`; dirty R12, x87 status, x87 tags, DS/ES
selectors or FS/GS bases → `entry_registers`. The x87 controls change status
and tags separately. Every register mutation must also fail the independent
assembly-record parity comparison after agreeing with each ptrace capture.
They do not turn an observed mismatch into a successful parity result.
Each mutation first requires a healthy start from the same test template.
Per-start image hashes and preparation controls are recorded under
`target/elf-loader-tests/` and the Cargo build output directory respectively.

```sh
cargo --offline fmt --all -- --check
cargo --offline clippy -p reverie-elf-loader --all-targets -- -D warnings
cargo --offline test -p reverie-elf-loader
```

Tests fail, rather than silently skip, when required personality/ptrace/procfs
facilities are unavailable. ASLR-on execution, other architectures, arbitrary
kernel/version portability and complete exec semantics are not claimed.
