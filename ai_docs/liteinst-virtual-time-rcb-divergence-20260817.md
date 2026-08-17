# LiteInst virtual-time RCB divergence from trampoline inode parsing

## 1. Headline

The ptrace-owned LiteInst hybrid PMU counts tracee-side LiteInst setup work as
guest retired conditional branch (RCB) progress. LiteInst's preload runtime
parses host-assigned inode numbers while the tracee PMU clock is enabled. The
number of decimal digits in those inode numbers therefore changes the guest's
virtual time.

**A guest's observable virtual time depends on which filesystem its fixtures
live on.**

The controlled divergence below used kernel-assigned trampoline memfd inodes.
The same parser also consumes the inode field for file-backed executable
mappings, including the guest fixture, so fixture placement changes the
absolute virtual-time baseline through the same mechanism.

This diagnosis is reproducible only with the `committed_time` comparator fix at
Hermit commit `fb22288f87564f270e8a2585e18f56d3a3458f47`. Without that fix,
the earlier host-perturbed scheduler `committed_time` field is reported first
and hides the virtual-time and timeslice divergence described here. A reader
testing unpatched `main` should not expect the canonical comparator to expose
this defect.

The exact binary used for the measurements had SHA-256:

```text
ae228dcbe61568e0076b3a6afab8a7fbf21a116892d8135cad6ebef818259f2a
```

## 2. Proof from inode width and virtual time

Six same-binary LiteInst runs printed `/proc/self/maps` and the Hermit run
summary. Every run created seven `liteinst2-trampoline` arenas, represented by
14 writable/executable alias lines. The only relevant varying input was the
decimal width of the kernel-assigned memfd inode:

```text
inode width 6: elapsed virtual global time 30,941,690ns (four runs)
inode width 5: elapsed virtual global time 30,934,970ns (-6,720ns)
inode width 3: elapsed virtual global time 30,921,530ns (-20,160ns)
```

At the default 10 virtual nanoseconds per RCB, one decimal digit changes the
final clock by exactly 672 RCBs:

```text
6,720ns / 10ns per RCB = 672 RCBs
20,160ns / 10ns per RCB = 3 * 672 RCBs
```

This is not one 672-RCB operation. During LiteInst setup,
`read_runtime_maps()` parses the accumulated trampoline mappings before and
after arena allocation. Each successful arena contributes two alias lines. An
eighth allocation attempt fails after the final "before" scan. The accumulated
alias counts `2, 4, 6, 8, 10, 12, 14` are therefore each parsed twice.
Empirically, parsing one additional decimal inode digit costs six RCBs per map
line:

```text
2 * (2 + 4 + 6 + 8 + 10 + 12 + 14) * 6 RCB = 672 RCBs
```

A two-digit width difference produces 1,344 RCBs. This explains both
quantized outcomes observed across the failing cells.

The relevant preload code is Reverie commit
`c261050cfd41bec67e31bfd0cf6f56be008d0ebb`:

- `reverie-liteinst/src/runtime.rs:1088-1115` reads `/proc/self/maps` and parses
  `inode` with `inode.parse()`.
- `reverie-liteinst/src/runtime.rs:1176-1229` scans executable mappings and
  reads the maps before and after each trampoline arena allocation.
- LiteInst2 commit `95ee5e6917fa33191eb41c3f1606ea8b03c1b78c`,
  `src/trampoline.rs:1509-1526`, creates the arena and tries executable aliases.

## 3. First divergent thread and turn

Across the retained exact-head failures, the first raw virtual-clock split is
always the same thread, scheduler turn, and resource:

```text
dettid 3
COMMIT turn 10
Path("/proc/self/maps"): R
```

A trace-level diagnostic bracketed the first differing interval between two
identical syscall boundaries:

```text
previous close: raw PMU clock 77527 / 77527
next openat:    raw PMU clock 105473 / 105485
logical RCBs:  105473 / 105485
```

The 12-RCB difference appears while the preload processes the previously read
maps and before it opens `/proc/self/maps` again. A second diagnostic trajectory
showed a 24-RCB first difference at the same boundary. Those differences then
accumulate to 672 and 1,344 RCBs respectively.

Three fresh canonical INFO-level repetitions of
`backend-parity-c/pid-probe`, verify mode, LiteInst backend produced
`FAIL / PASS / PASS`. The failing repetition again began at dettid 3, turn 10,
and finished 6,720ns apart. The trace-level runs were used only to inspect raw
PMU values: TRACE adds syscall-span address fields that cause an earlier,
unrelated INFO comparison mismatch.

The raw PMU clock and Detcore's logical RCB count advance by the same amount.
This rules out arithmetic double-counting in Detcore. The additional branches
really execute in the tracee-side LiteInst setup path and are then counted as
guest progress.

## 4. Guest-visible consequence

The divergence is not confined to the final report. In
`c-programs/socket-timestamp-edge-cases`, verify mode, LiteInst backend, the two
guest executions observed different `CLOCK_REALTIME` values:

```text
run 1: clock_gettime(CLOCK_REALTIME) -> tv_nsec: 29227310
run 2: clock_gettime(CLOCK_REALTIME) -> tv_nsec: 29220590
```

The difference is exactly 6,720ns, the same value later printed in the final
global-time and timeslice statistics. Thus tracee-side setup work changes a
clock value returned to the guest.

## 5. Virtual-time propagation chain

The observed value follows one continuous code path:

1. `detcore/src/lib.rs:405-435` reads the cumulative PMU clock, computes
   `delta_rcbs`, adds it once to `thread_logical_time`, and records the new
   cumulative baseline.
2. `detcore/src/lib.rs:2468-2472` closes the final timeslice using that actual
   `thread_logical_time`.
3. `detcore/src/tool_global.rs:566-578` reads the actual `GlobalTime` for the
   final and elapsed virtual-time fields.
4. `detcore/src/syscalls/time.rs:209-222` obtains the same virtual clock for
   `clock_gettime` and writes it into guest memory.

The ptrace PMU configuration counts tracee user code and excludes kernel,
guest, and hypervisor execution. The non-resetting per-thread PMU clock is in
Reverie `reverie-ptrace/src/perf.rs:216-225` and
`reverie-ptrace/src/timer.rs:753-779,866-868`. It is not disabled around the
LiteInst preload's setup or hook code.

## 6. Falsified HashMap-seeding hypothesis

Nondeterministic Detcore HashMap seeding was investigated and ruled out for
this failure.

Hermit uses `LiteinstBackend::run_host_with_preload` at
`hermit-cli/src/lib.rs:1677-1683`. Reverie documents this as the ptrace-owned
hybrid: ptrace owns the sole Tool and GlobalTool, while the preload contributes
only dynamic site installation and injected hot-site traps
(`reverie-liteinst/src/backend.rs:203-237`). Detcore's HashMaps therefore run
in the tracer/coordinator process and retire no branches on the tracee PMU.

The preload artifact has no Detcore dependency. The exact tracee-side interval
that introduces the RCB difference executes
`reverie-liteinst/src/runtime.rs:1088-1229`, which uses vectors, strings, and
integer parsing. Pinned LiteInst2's corresponding map processing also uses a
`Vec` and sorting; it contains no HashMap or HashSet in this path.

There is no fixed standard-library HashMap seed mode in this configuration,
but no such experiment is needed to distinguish these hypotheses: the HashMaps
are outside the measured process, while changing only the observed memfd inode
width predicts the virtual-time result exactly, including the 672-RCB quantum
and its integer multiples.
