# KVM Guest-Kernel ABI Design for Hermit

Status: design proposal, 2026-07-23
Author: agent hermit-110 (task `impl-kvm-guest-kernel-design`)
Scope: what is needed to make `hermit --backend kvm` run real Linux ELF
binaries, following gVisor's guest-kernel model but preserving Hermit/Detcore's
determinism semantics.

## Executive summary

`reverie-kvm` (reverie rev `6981ac0`) can create a one-vCPU VM, expose bounded
guest-physical memory, apply a deterministic CPUID policy, and turn a guest
`vmcall` into a typed `reverie::syscalls::Syscall` that is dispatched to a normal
`reverie::Tool` (including Detcore). It is **not** an execution backend: the
guest runs in **16-bit real mode**, programs are hand-assembled, and there is no
ELF loader, virtual memory, process lifecycle, signals, or timers. Running
`/bin/true` fails by construction (hermit issue #198).

The gap between "route one hand-written `vmcall`" and "run a dynamically linked
PIE" is a **guest-kernel ABI layer**. gVisor solves the same problem with its
`ring0` package (a tiny in-guest kernel) plus its `sentry` (a userspace Linux
personality). Hermit needs the `ring0`-equivalent transport, but explicitly
**not** the full sentry: Detcore does not reimplement Linux syscalls — it
*forwards* them to the host kernel and sanitizes nondeterministic results. So
Hermit's guest kernel is a thin trap-and-transport shim, and the syscall
"implementation" is Detcore + a forward-to-host executor that `reverie-kvm`
already abstracts as `SyscallExecutor`.

This document specifies that shim, maps every piece to existing code, and gives
a milestone plan with assurance levels.

## 1. Current state (what exists today)

### reverie-kvm @ 6981ac0 (`reverie-kvm/src/`)

| File | What it provides | Limit |
| --- | --- | --- |
| `vm.rs` | `KvmBackend`: opens `/dev/kvm`, one VM, one vCPU, one memory slot at GPA `0x1000`; enables `KVM_CAP_EXIT_HYPERCALL` for hypercall #12; picks `vmcall`/`vmmcall` from CPUID; `install_syscalls` writes a real-mode program of `mov`+`vmcall`; `run`/`run_with_tool`. | **Real mode only** (`cs.base=ds.base=0`, `rflags=2`); no paging, GDT/IDT, EFER, ring0/ring3, MSR_LSTAR. Programs are hand-assembled bytes. |
| `syscall.rs` | `SyscallRequest` = 7×u64 frame (number + 6 args); `into_syscall()` decodes through Reverie's full `Sysno` table, rejecting unknown numbers. | Transport frame is written by the host, not produced by a guest `SYSCALL` trap. |
| `runtime.rs` | `run_with_tool::<T: Tool>()`: bridges each `vmcall` to `Tool::handle_syscall_event`; implements `Guest`, `Stack`, `GlobalRPC`, `inject`, `tail_inject`, thread state. `SyscallExecutor` trait supplies Linux semantics for injected/unsubscribed calls. | `set_timer`/`read_clock` return `Unsupported`. Single `GUEST_PID=1`, one thread, one stack page. No process lifecycle beyond `Hlt` → exit. |
| `memory.rs` | `GuestMemory`: one `mmap(MAP_SHARED)` region shared host/guest; `MemoryAccess` impl (read/write vectored). | One contiguous region; no per-process address spaces, no page-table translation (GVA≈GPA). |
| `cpuid.rs` | `CpuidPolicy` masking RDRAND/RDSEED/TSX/AVX-512 via `KVM_SET_CPUID2`. | Static vCPU policy, not a per-instruction `handle_cpuid_event`. |

### Hermit wiring (`hermit-cli/`)

- `Cargo.toml` pins `reverie`, `reverie-kvm`, `reverie-ptrace` at `6981ac0`.
  (`detcore`/`detcore-model` manifests still name `f6bcc06` — version skew to
  reconcile; `f6bcc06` predates `reverie-kvm` entirely.)
- `Backend::Kvm` is CLI-selectable (`--backend kvm`); `run.rs` maps the string.
- `lib.rs::run_kvm` constructs `KvmBackend::new()` to prove the VM boots, then
  returns an accurate "no Linux execution personality" error (issue #198).
- `run_with_backend` always builds `reverie_ptrace::TracerBuilder::<Detcore>`.
  A KVM path needs a parallel launcher driving `Detcore` through
  `KvmBackend::run_with_tool::<Detcore, _>`.

## 2. How gVisor intercepts syscalls via KVM

gVisor's `Platform` interface (`pkg/sentry/platform/platform.go`) is three
methods: `NewAddressSpace`, `NewContext`, and `Context.Switch(as, arch.Context)`
plus `AddressSpace.MapFile`/`Unmap`. The KVM platform implements this so the
**sentry acts as both guest OS and VMM**.

Mechanism (gVisor terms; see `pkg/sentry/platform/kvm/` and `pkg/ring0/`):

- **ring0** is a minimal in-guest kernel: it builds the GDT/IDT/TSS
  (`kernel_amd64.go`), 4-level page tables (`pkg/ring0/pagetables`), enables
  long mode, and installs a `SYSCALL` entry via `MSR_LSTAR`
  (`startGo`→`wrmsr(_MSR_LSTAR, addrOfSysenter())`). `entry_amd64.s`'s
  `sysenter` (the LSTAR target) and `exception` trampolines save the interrupted
  **user register frame** (layout = `linux.PtraceRegs`, stored in
  `CPU.registers`) and return a trap **vector** (e.g. `Syscall`=256) rather than
  a value. Its *only* job is to shuttle the frame + vector back to host code.
- **bluepill** (`bluepill_amd64.go`, `bluepill_unsafe.go`) lets the sentry's own
  thread transparently descend into guest ring0: `bluepill(c)` faults into a
  signal handler (`sighandler`→`bluepillHandler`), which transplants the
  interrupted `ucontext` GPRs into the vCPU (`bluepillArchEnter`) and loops on a
  raw `ioctl(KVM_RUN)`. Execution continues *inside the VM at the same Go
  instruction stream*. `bluepillHandler` classifies `runData.exitReason`
  (`HLT`, `MMIO`/hypercall, `IRQ_WINDOW`, `EINTR`, `EFAULT`, fatal). The reverse
  transition is HLT-based: when sentry code in guest ring0 needs a real host
  syscall, `KernelSyscall` executes `HLT` → `KVM_EXIT_HLT` → the signal handler
  returns so the host re-runs the `SYSCALL` natively. This avoids
  `KVM_SET_REGS`/`KVM_GET_REGS` on the hot path.
- **Context.Switch** (`machine_amd64.go::vCPU.SwitchToUser`, `context.go`)
  dispatches a guest userspace frame. When guest ring3 executes `SYSCALL` it
  vectors to the ring0 `sysenter` trampoline, which returns vector `Syscall`;
  `SwitchToUser` treats it as the fast path and returns to the sentry task loop
  **without a VM-exit** (the sentry keeps running in guest ring0 for speed). The
  task loop reads guest GPRs from `arch.Context64`, takes RAX as the number, and
  indexes the sentry syscall table (`pkg/sentry/syscalls/linux`). Other vectors
  map to `ErrContextSignal` / `ErrContextInterrupt` / page-fault / CPUID paths.
- **AddressSpace.MapFile/Unmap** (`address_space.go`) edit the guest page tables
  so a host file range backs a guest virtual range; population is lazy — a guest
  `PageFault` vector triggers `mm.HandleUserFault` → `MapFile`, which
  `MapInternal`s the backing host `memfd` (`pgalloc.MemoryFile`, the guest's
  "physical memory") into the sentry, ensures a KVM memory slot
  (`KVM_SET_USER_MEMORY_REGION`), and installs GVA→GPA entries via
  `pageTables.Map`.

Crucially, in gVisor the syscall is **implemented by the sentry** (a full Linux
personality: VFS, gofer/directfs filesystem, netstack, `mm`, `loader`). The
guest never touches the host kernel directly:

- **exec**: `pkg/sentry/loader` parses the ELF in userspace, maps segments and
  the PIE/interpreter into a fresh `mm.MemoryManager`, builds the initial stack
  (argv/envp/auxv), and sets the entry `rip` — no host `execve` of the guest.
- **mmap**: `mm` package tracks VMAs; `AddressSpace.MapFile` lazily backs them;
  file-backed pages come from host fds held by the sentry, not the guest.
- **file I/O**: `open`/`read`/`write` go through the sentry VFS to a gofer (9P)
  or directfs host fd; the guest's syscall never reaches the host kernel as a
  syscall.

## 3. The design choice: transport like gVisor, semantics like Detcore

gVisor and Hermit sit at opposite ends of a spectrum:

| | gVisor sentry | Hermit / Detcore |
| --- | --- | --- |
| Who implements a syscall | Sentry reimplements Linux in userspace | Host Linux kernel executes it; Detcore only sanitizes the result |
| Guest → host kernel | Never (isolation is the goal) | Yes, via injection (determinism is the goal) |
| What the guest kernel must contain | A whole OS personality | A trap + a transport |

One consequence of this inversion is worth stating up front. In gVisor the
*sentry* runs inside guest ring0 and only VM-exits (via HLT) when it needs a
*host* syscall; a guest-app `SYSCALL` is the fast path that stays in the VM. In
Hermit the *tool* (Detcore) runs on the **host**, so every subscribed guest
syscall must VM-exit to reach it — Hermit's normal path is exactly the exit that
gVisor optimizes away. The bluepill "transplant + stay in guest" trick therefore
does not apply to Hermit's hot path; the design should instead minimize per-exit
cost (batch/keep the `vmcall` frame transport, avoid `KVM_GET_REGS`/`SET_REGS`
where the frame already carries the register state). This is inherent to running
the determinism policy out-of-VM and is analogous to ptrace's per-syscall stop.

Detcore forwards after sanitizing — confirmed in source: e.g.
`detcore/src/syscalls/time.rs` and `files.rs` call `guest.inject(Syscall::…)`
(24 `inject` sites, 53 `guest.memory` sites) and return the host result. In the
KVM backend `guest.inject` already routes to `SyscallExecutor::execute`.

**Therefore Hermit's guest kernel does not need a sentry.** It needs only what
turns a real ELF's `SYSCALL` instruction into a `SyscallRequest` and back. The
"implementation" of the syscall is: Detcore decides → (sanitize | forward) →
forward = execute on the host on behalf of the guest, exactly what ptrace
injection does today. We borrow gVisor's **ring0 transport model** and reject
its **sentry personality**.

## 4. The minimal guest-kernel ABI layer

Ordered from lowest to highest layer. Items marked *(reuse)* already exist in
`reverie-kvm`; *(new)* is net-new work.

### 4.1 CPU bring-up: real mode → 64-bit long mode *(new)*

Replace `install_real_mode_program`'s real-mode setup with protected/long-mode
`sregs`: set `CR0.PE|PG`, `CR4.PAE`, `EFER.LME|LMA|SCE`, load a flat GDT with a
ring0 code/data pair and a ring3 code/data pair, and point `CR3` at a 4-level
page-table root built in guest memory. This is the single biggest missing
primitive; everything else depends on ring3/ring0 separation and paging.

### 4.2 Guest page tables + per-process address space *(new)*

Introduce an `AddressSpace` abstraction analogous to gVisor's: a page-table tree
in guest physical memory mapping guest-virtual → guest-physical, plus a
host-side allocator handing out guest-physical frames from the `GuestMemory`
region (grow `GuestMemory` to multiple slots / on-demand `KVM_SET_USER_MEMORY_
REGION`). `GuestMemory` today assumes GVA==GPA and one region; it must gain
GVA→GPA translation (walk the guest page tables, or keep a shadow map host-side)
so that `Guest::memory()` reads still work after the ELF is mapped at its real
virtual addresses.

### 4.3 SYSCALL trap + transport *(partly reuse)*

Install a ring0 `SYSCALL` entry via `MSR_LSTAR`/`MSR_STAR`/`MSR_SFMASK`. The
trampoline (a few dozen bytes of guest code placed at a fixed kernel VA):

1. saves the user register frame (the `libc::user_regs_struct` shape already
   used in `runtime.rs::kvm_registers`);
2. writes `rax` + `rdi,rsi,rdx,r10,r8,r9` into a per-thread `SyscallRequest`
   frame (reuse `syscall.rs` layout);
3. issues `vmcall`/`vmmcall` with the frame GPA (reuse the transport opcode and
   `KVM_EXIT_HYPERCALL` decode in `vm.rs`/`runtime.rs`);
4. on return, loads the result into `rax` and `sysret`s back to ring3.

This keeps `SyscallRequest`, `into_syscall`, the vmcall decode, and the
`run_with_tool` dispatch loop essentially unchanged — the difference is the
frame is produced by a *real guest `SYSCALL`* rather than pre-installed.
(Alternative: skip the frame and read args directly from the saved register
frame; the frame indirection is convenient but not required once we have a real
trampoline.)

### 4.4 ELF loader *(new, host-side)*

A host-side loader (mirror gVisor `pkg/sentry/loader`, or reuse Reverie's
process bootstrap): parse the ELF, allocate guest-physical frames, map PT_LOAD
segments (and the PIE base / `ld.so` interpreter) into the new address space's
page tables, build the initial stack (argv, envp, auxv incl. `AT_RANDOM`,
`AT_SYSINFO_EHDR` if a vDSO is provided), and set entry `rip`/`rsp`. No host
`execve`; the guest ELF's own `SYSCALL`s are trapped from the first instruction.

### 4.5 Memory-management syscalls *(new)*

`mmap`/`munmap`/`mremap`/`brk` must edit guest page tables rather than forward
blindly. Two options:
- **(a) host-managed:** the KVM kernel services these itself (allocate frames,
  update page tables, back file-mapped ranges by reading host files via the
  executor). This mirrors gVisor and is required for `MAP_ANONYMOUS` and stack
  growth.
- **(b) forward + reflect:** forward to host `mmap` to obtain semantics/errno,
  then reflect the resulting mapping into guest page tables backed by fresh
  guest frames, copying file contents through the executor.
Detcore already intercepts `handle_mmap`/`munmap`/`mremap`; the KVM layer must
supply the address-space side-effect that ptrace gets for free (the tracee's
own kernel does it).

### 4.6 Process/thread lifecycle *(new, hard)*

`clone`/`fork`/`execve`/`exit`/`wait4` and futexes. Detcore already models these
(`handle_clone_family`, `handle_futex*`, `handle_wait4`, `handle_execveat`) and
runs a deterministic scheduler that serializes threads onto one logical CPU. The
KVM backend must map each guest thread to a vCPU (or multiplex threads onto one
vCPU by swapping register frames + `CR3`, which fits Detcore's
single-logical-CPU model better and is closer to how ptrace serializes). This is
where `run_with_tool` must grow from "one thread until `hlt`" to a scheduler
loop driven by Detcore's `GlobalState`.

### 4.7 Signals, timers, CPUID/RDTSC events *(new)*

- **Timers/preemption:** `set_timer`/`set_timer_precise`/`read_clock` currently
  return `Unsupported`. Deterministic preemption needs PMU RCB counting
  delivered as a vCPU exit (KVM `KVM_CAP_PMU` / debug facilities) mapped to
  Detcore's `handle_timer_event`. This is the same RCB machinery ptrace uses.
  In gVisor, preemption uses a dedicated *bounce* signal: the scheduler calls
  `vCPU.NotifyInterrupt`→`bounce`, which `Tgkill`s the vCPU thread with a
  reserved signal, forcing `KVM_RUN` to return `EINTR`; the handler injects a
  `VirtualizationException` vector that `SwitchToUser` reports as
  `ErrContextInterrupt`. Hermit's equivalent is delivering a PMU RCB overflow as
  a vCPU exit and surfacing it as `handle_timer_event`.
- **Signals:** delivered by the guest kernel itself (as gVisor's sentry does):
  build a signal frame on the guest stack and vector `rip` to the handler — not
  via host signals — gated by Detcore's `handle_signal_event`.
- **CPUID/RDTSC:** enable `KVM_CAP_EXIT_CPUID` / RDTSC exiting to route to
  `handle_cpuid_event`/`handle_rdtsc_event` for full per-instruction control (the
  static `CpuidPolicy` covers the common case today).

## 5. What to reuse vs. build

| Concern | Reuse from reverie-kvm | Borrow from gVisor (concept) | Build new |
| --- | --- | --- | --- |
| VM/vCPU/`/dev/kvm` | `KvmBackend`, CPUID policy | — | multi-slot memory, long-mode sregs |
| Guest memory | `GuestMemory`, `MemoryAccess` | pagetables layout | GVA→GPA translation, frame allocator |
| Syscall transport | `SyscallRequest`, vmcall decode, `run_with_tool` | ring0 `MSR_LSTAR` trampoline | real `SYSCALL` trap trampoline |
| Syscall semantics | `SyscallExecutor` (= forward to host) + Detcore | *(reject sentry reimpl)* | — |
| Tool bridge | `Guest`/`Stack`/`GlobalRPC`/`inject`/`tail_inject` | Context.Switch shape | scheduler loop, per-thread frames |
| ELF/exec | — | `pkg/sentry/loader` | host-side ELF loader |
| mmap/brk | Detcore `handle_mmap` etc. | `AddressSpace.MapFile` (lazy) | page-table side effects |
| threads | Detcore scheduler + `handle_clone_family` | vCPU-per-thread / frame-swap | thread↔vCPU multiplexing |
| timers/signals | — | signal-frame injection, PMU exits | RCB preemption, signal delivery |

## 6. Milestones (with target assurance levels)

- **M0 — Long-mode "hello" (L0):** bring up long mode + paging + a ring0
  `SYSCALL` trampoline; run a *hand-written* 64-bit guest that issues `write`
  and `exit_group` via real `SYSCALL` (not pre-installed vmcalls), forwarded by
  `SyscallExecutor`. Proves the trap/transport rewrite. Unit + `/dev/kvm` test.
- **M1 — Static ELF (L0→L1):** host-side ELF loader for a *statically linked,
  single-threaded* binary (e.g. a musl `true`/`echo`); map segments, build the
  stack, forward all syscalls to host via the executor. Target: `hermit
  --backend kvm run -- ./static_echo` prints real output.
- **M2 — Detcore integration (L1):** drive `Detcore` (not a toy Tool) through
  `run_with_tool`; sanitize time/rng/cpuid; add a `run_kvm` dispatch in
  `hermit-cli` parallel to the ptrace launcher. Compare output/log vs ptrace on
  the same static binary.
- **M3 — Dynamic PIE (L1):** support `ld.so`, `mmap`/`brk` with page-table side
  effects; run a dynamically linked `/bin/echo`.
- **M4 — Threads + preemption (L2):** thread↔vCPU multiplexing, futexes, PMU
  RCB preemption → `handle_timer_event`; `--strict --verify` bitwise-identical
  repeat on a multithreaded guest.
- **M5 — Signals/timers + stress (L3/L4):** signal-frame delivery, timerfd/
  itimer; L2/L3 repeated 20× without divergence.

## 7. Risks and open questions

1. **Thread model.** vCPU-per-thread is closest to real hardware but fights
   Detcore's single-logical-CPU serialization; frame-swap on one vCPU matches
   Detcore but is more code. Recommendation: frame-swap on one vCPU (mirrors how
   ptrace serializes) — resolve early, it shapes M4.
2. **Forward-to-host determinism.** Forwarding real syscalls to the host has the
   *same* filesystem/network nondeterminism ptrace has (see the existing
   `InternalIOPolling` note: readiness polled against live host fds breaks
   `--strict --verify` for multi-process guests). KVM inherits, not fixes, this;
   record/replay remains the path for such guests.
3. **Memory sharing vs isolation.** `GuestMemory` is `MAP_SHARED` so the host
   reads guest memory directly — convenient and needed for `Guest::memory()`,
   but once page tables exist the host must translate GVA→GPA on every access.
4. **PMU under virtualization.** RCB counting may be unavailable/inaccurate in
   nested VMs or restricted hosts (already a documented Hermit limitation). M4/M5
   are the most hardware-sensitive.
5. **vDSO.** Real glibc expects a vDSO (`clock_gettime`, `getcpu`). Either
   provide a minimal deterministic vDSO page or ensure the fallback `SYSCALL`
   path is always taken.
6. **Version skew.** Reconcile `detcore`/`detcore-model` (`f6bcc06`) with
   `hermit-cli` (`6981ac0`) before wiring a real KVM launcher.

## 8. Conclusion

The minimal guest kernel Hermit needs is **not** a gVisor sentry — it is a
long-mode ring0 trap-and-transport shim plus a host-side ELF loader and
address-space manager. Everything above the syscall boundary (semantics,
determinism, scheduling) is already provided by Detcore, and the
forward-to-host path is already abstracted by `reverie-kvm`'s `SyscallExecutor`.
The highest-leverage first step is M0: replace the real-mode, pre-installed-
vmcall prototype with a 64-bit paged guest whose real `SYSCALL` instruction is
trapped through a ring0 trampoline. That single change converts `reverie-kvm`
from a transport demo into the foundation of a Linux execution backend.
