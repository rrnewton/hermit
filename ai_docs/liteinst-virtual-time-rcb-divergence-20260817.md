# LiteInst virtual-time RCB divergence from tracee-side setup

## Status and decision boundary

This document records an established product-semantics defect and scopes a
possible fix. It does **not** authorize implementation.

Tonight's comparator corrections, scorecard provenance checks, selection-schema
work, and monitoring-gate repairs changed how evidence is measured, attributed,
or selected. They did not change the Linux values returned to a guest. This
defect is different: correcting it changes LiteInst virtual-time accounting and
therefore changes values returned by guest calls such as `clock_gettime`.
It also makes LiteInst's existing RCB-derived evidence history stale. Choosing
that new backend behavior and accepting the re-baseline is an owner decision.

## Root cause

Hermit's LiteInst path is a ptrace-owned hybrid. Ptrace owns the DetCore Tool,
the thread PMU clock, and PMU preemption. The preload runtime executes inside
the tracee to prepare LiteInst instrumentation.

The ptrace PMU counts that tracee-side LiteInst setup work as guest retired
conditional branch (RCB) progress. DetCore cannot distinguish those branches
from application branches when it reads the cumulative PMU clock, so setup work
advances the guest's logical time.

The established variable input is the decimal inode field parsed from
`/proc/self/maps`. Host-assigned inode width changes how many branches the
preload executes while parsing map entries, and that difference is charged to
the guest.

## Quantized proof

Six same-binary LiteInst runs created the same seven trampoline arenas. The
only relevant varying input was the decimal width of their kernel-assigned
memfd inode numbers:

```text
inode width 6: elapsed virtual global time 30,941,690ns
inode width 5: elapsed virtual global time 30,934,970ns  (-6,720ns)
inode width 3: elapsed virtual global time 30,921,530ns (-20,160ns)
```

At the default 10 virtual nanoseconds per RCB, each additional decimal digit
contributes exactly 672 RCBs:

```text
6,720ns / 10ns per RCB = 672 RCBs
20,160ns / 10ns per RCB = 3 * 672 RCBs
```

`read_runtime_maps()` parses the accumulated trampoline aliases before and
after arena allocation. Each arena adds two aliases. The accumulated alias
counts `2, 4, 6, 8, 10, 12, 14` are each parsed twice, and one additional inode
digit costs six RCBs per map line:

```text
2 * (2 + 4 + 6 + 8 + 10 + 12 + 14) * 6 RCB = 672 RCBs
```

The first split is consistently:

```text
dettid 3
COMMIT turn 10
Path("/proc/self/maps"): R

previous close: raw PMU clock 77527 / 77527
next openat:    raw PMU clock 105473 / 105485
```

The 12-RCB difference appears while the preload processes the previous maps
contents and before it opens `/proc/self/maps` again. Runs with a two-digit
inode-width difference first split by 24 RCBs at the same boundary.

## Guest-visible consequence

The divergence reaches guest memory. In
`c-programs/socket-timestamp-edge-cases`, two LiteInst executions returned
different `CLOCK_REALTIME` values:

```text
run 1: clock_gettime(CLOCK_REALTIME) -> tv_nsec: 29227310
run 2: clock_gettime(CLOCK_REALTIME) -> tv_nsec: 29220590
```

The 6,720ns difference is the same inode-width-dependent delta later reported
in final virtual-time statistics.

The propagation is one continuous path:

1. `detcore/src/lib.rs:405` reads the cumulative PMU clock, computes
   `delta_rcbs`, and adds it to `thread_logical_time`.
2. `detcore/src/lib.rs:2468` closes the final timeslice using that logical time.
3. `detcore/src/tool_global.rs:566` publishes final and elapsed virtual time.
4. `detcore/src/syscalls/time.rs:209` obtains the same virtual clock for
   `clock_gettime` and writes it to guest memory.

The same logical time also feeds scheduler events, preemption deadlines,
process/thread CPU accounting, sleeps and timers, and run summaries. A repair
must keep those consumers consistent; subtracting setup time only from
`clock_gettime` would create a second defect.

## Existing execution boundaries

The exact LiteInst setup boundary already exists in Reverie:

- The preload emits `HandshakeBegin` immediately before
  `prepare_instrumentation()`.
- It emits `HandshakeReady` immediately after instrumentation preparation.
- `reverie-ptrace` validates both traps and tracks
  `PreExec -> Waiting -> Bootstrap -> Ready`.
- Ptrace also owns an executable-entry guard that prevents application entry
  before the required handshake completes.

`past_global_first_execve` is not the right boundary: DetCore sets it before
the dynamic loader invokes the LiteInst preload constructor. Waiting until the
main executable entry would be broader than the established defect because it
would exclude dynamic-loader work and every preload or application constructor,
not only LiteInst instrumentation preparation.

The narrow scope is therefore the validated `HandshakeBegin` to
`HandshakeReady` interval.

## What a correct fix would require

A DetCore-only conditional is too late. By the time
`update_logical_time_rcbs()` reads the clock, the PMU value no longer records
which branches belonged to LiteInst setup.

A backend-local repair would need the ptrace timer to present a monotonic
logical RCB clock that excludes the validated setup interval. It must also
coordinate the separate PMU overflow counter used for preemption:

1. Preserve all raw PMU progress before `HandshakeBegin`.
2. Freeze the logical value returned by `read_clock()` during `Bootstrap`.
3. Accumulate the raw begin-to-ready delta into a persistent excluded offset.
4. Suspend the overflow counter during the interval.
5. Retain the latest timer request made while setup syscalls are handled.
6. Rearm that request at `HandshakeReady` relative to the adjusted clock.
7. Refuse on missing, repeated, or out-of-order markers and on PMU operation
   failure.
8. Ensure cancellation and activation failure cannot leave the clock or timer
   disabled.

Disabling only the counting clock is insufficient: DetCore handles setup
syscalls and may rearm the overflow timer, and a timer signal can currently be
handled before the LiteInst runtime reaches `Ready`. Resetting the raw clock is
also insufficient because DetCore requires a cumulative, monotonic
`committed_clock_value`.

This is a bounded Reverie change, principally in
`reverie-ptrace/src/timer.rs` and the LiteInst handshake path in
`reverie-ptrace/src/task.rs`, followed by a Hermit Reverie-pin update and
requalification. It is not a one-line gate.

There is no reusable ptrace PMU pause API in another backend. Existing paths
avoid the contamination in different ways:

- e9patch preprocessing finishes before the ptrace tracee starts.
- DBT supplies `DbtGuest` with an application branch count and disables PMU
  maximum-timeslice preemption.
- SaBRe uses its backend-specific thread-local clock-read path rather than the
  ptrace-owned PMU clock.

## Evidence and replay blast radius

A handshake-scoped repair changes LiteInst semantics, not ptrace, SaBRe, DBT,
or KVM semantics. Those other backends' existing evidence remains meaningful.

Every existing LiteInst artifact whose claim depends on RCB-derived time goes
stale, including:

- exact-head qualification receipts;
- canonical INFO logs containing logical time, RCB counts, or scheduler times;
- guest-visible virtual-time results;
- run summaries and CPU-time observations derived from logical time; and
- recorded schedule or preemption traces.

All currently green LiteInst cells and all proposed LiteInst promotions must be
re-run at the final Hermit and Reverie identities. Under the project's
exact-head admission rule, mixed-mode ptrace and SaBRe siblings also need fresh
receipts for a promotion even though their backend semantics are unchanged.

### Missing schedule semantics version

LiteInst schedule compatibility must be handled as part of any PMU accounting
change, not afterward. `PreemptionRecord` currently has no top-level semantics
version or backend identity. Replay compares recorded branch-event counts and,
when present, event end times. An old LiteInst schedule can therefore be read
under the new PMU accounting and silently compared against different branch
counts instead of refusing at load time.

That is the same evidence-identity failure shape as crediting a verdict from
one binary to another: the artifact parses, but its producing semantics are not
the consuming semantics. Any implementation must either add sufficient
artifact identity and refuse the old LiteInst schedule or deliberately provide
a legacy counting mode selected by that identity. Preserving old schedules by
accident is not a valid compatibility policy.

## Scorecard value

At Hermit main `da85fa31e1c667a534d12414efbf94f738565174`, the published
scorecard has 282 green compatibility cells and the required execution plan has
284 cells.

The defect directly affects 20 comparable LiteInst cells:

- 2 currently green LiteInst cells; and
- 18 red LiteInst candidates with historical repeated evidence.

Of those 18 red `verify` modes, 17 have every non-LiteInst sibling clean in the
latest three-round audit. Those modes contain 30 clean ptrace/SaBRe siblings.
If the owner authorizes the semantics change and all cells requalify at the
final head, the current mode-wide selection schema could therefore promote:

```text
17 LiteInst cells + 30 clean ptrace/SaBRe siblings = 47 cells
scorecard:     282 -> 329 green
required plan: 284 -> 331 cells
```

The three promotions refused tonight account for 9 of those 47 cells. Another
14 mixed `verify` modes, containing 38 cells, are blocked by the same LiteInst
defect. `bin-c/posix-timer-test` is the eighteenth LiteInst candidate but would
remain excluded because its SaBRe sibling has an independent product failure.

## Required validation for an authorized implementation

At minimum:

1. Prove logical-clock monotonicity while variable amounts of tracee-side setup
   work execute between `HandshakeBegin` and `HandshakeReady`.
2. Prove precise and imprecise timer deadlines retain their remaining guest RCB
   distance across that interval.
3. Prove setup branches create no guest Branch `SchedEvent`, logical CPU-time
   advance, or guest-visible time advance.
4. Exercise missing, repeated, and out-of-order markers, PMU failures,
   cancellation, and activation failure; every case must fail closed and leave
   no disabled counter or pending signal.
5. Establish the schedule-artifact semantics-version refusal or the explicitly
   selected legacy behavior.
6. Repeat all 18 LiteInst candidates three times at one exact Hermit commit,
   Reverie commit, and binary SHA, requiring canonical comparison and nonzero
   INFO on both sides.
7. Re-run the two already-green comparable LiteInst cells and ordinary full
   validation at that same final identity.

## Falsified HashMap-seeding hypothesis

Nondeterministic DetCore HashMap seeding does not explain this divergence.

Hermit uses `LiteinstBackend::run_host_with_preload`. Ptrace owns the sole
DetCore Tool and GlobalTool, so DetCore's HashMaps execute in the tracer or
coordinator process and do not retire branches on the tracee PMU. The preload
artifact has no DetCore dependency. The tracee-side interval that introduces
the RCB difference uses vectors, strings, sorting, and integer parsing in the
LiteInst runtime; the relevant `/proc/self/maps` path contains no HashMap or
HashSet. Changing only inode width predicts the virtual-time result exactly,
including the 672-RCB quantum and its integer multiples.

## Evidence identities

- Diagnosis preserved by Hermit commit `6d1bc48a6b`.
- Measurement binary SHA-256:
  `ae228dcbe61568e0076b3a6afab8a7fbf21a116892d8135cad6ebef818259f2a`.
- Reverie diagnosis source commit:
  `c261050cfd41bec67e31bfd0cf6f56be008d0ebb`.
- LiteInst2 trampoline source commit:
  `95ee5e6917fa33191eb41c3f1606ea8b03c1b78c`.
