---
name: hermit-debugging
description: "Debug Hermit and Detcore test cells, validation failures, nondeterminism, hangs, syscall gaps, and scheduling from exact-SHA evidence and Hermit's logs before reading source. Use whenever a guest behaves unexpectedly, a check or --verify result is suspicious, a run hangs, or tests/DEBUGGING.md needs a current investigation entry."
---

# Debugging Hermit Runs

> **Don't break the demos.** If a fix touches a demo, verify the demo still runs
> GREEN before landing — demo-touching commits require an adversarial green-demo
> review, not just code-review.

**Thesis: reach for hermit's logs before you reach for the source.** Detcore
emits a rich, structured trace of every scheduling decision, syscall, and
virtual-time advance. Most "why did this diverge / hang / behave oddly?"
questions are answered by reading that trace or diffing two of them — *not* by
reading `scheduler.rs`. Read the code only once the log has told you *where* to
look.

All commands below assume the repo root and the release binary
`target/release/hermit` (use `target/debug/hermit` if that is what you built).
On Meta devservers prefix network-touching commands with `with-proxy`.

## Establish what actually failed

Start from one exact cell: lane, manifest bucket, test id, mode, and backend.
Bind every observation to the full Hermit SHA and tree, the binary path,
`binary_build_sha` and binary hash when available, `run_id`, UTC time,
host/kernel/toolchain, exact command, working directory, environment, timeout,
exit status, and retained result/log paths. If the binary may be stale, rebuild
it in the checkout at that SHA before attributing behavior to the source. For a
claimed regression, run the same bounded probe against a clean current-main
control.

Read existing evidence before rerunning. In a `dev-hermit` workspace, use the
timestamped validate LEDGER and `ci-hub validate-status` for run-level evidence.
Use `ci/compat-envelope/cells.json` for the cell's checked-in status,
`last_tested`, measurement state, and pressure/validate observations; remember
that this file is a projection and its own `projection.refreshed_at` and
`rows_read` say whether it is current. If the optional `projection` block is
absent, freshness is unknown; use the LEDGER rather than assuming the file is
current. `last_tested` has no timestamp and cannot replace the LEDGER clock.
Join run-level and per-cell evidence by `run_id` plus the full cell identity.
Follow the artifact directory named by a non-PASS harness line for the typed
result, verify report, captures, and logs.

Keep the repository's outcomes distinct. `FAIL`, `ERROR`, `HOST-INAPPLICABLE`,
`never-measured`, `measured-no-verdict`, a missing row, and an incomplete run do
not mean the same thing, and none may be relabeled PASS. A failure under
contention is a real defect: record the observed width/load, rerun the same cell
alone to isolate the mechanism, and keep both results. A solo pass does not
erase the contended failure or make it unclassifiable.

Use the project's existing vocabulary and exact identifiers. If the repository
has no established term for an observation, describe it in plain language; do
not coin a new class or mechanism name.

## Prove that the check observes its claim

Before trusting a test, assertion, gate, watcher, or metric, ask: **can this
check fail for the reason its name gives?** Trace its input through selection,
execution, and verdict, then exercise both directions:

1. State the exact behavior the check claims to distinguish.
2. Prove the check is enabled, selected, schedulable, and reaches the relevant
   assertion. A disabled/manual-only dependency, early return, skip, missing
   backend, empty selection, or allowlist exemption is a coverage gap, not a
   green result.
3. Make one controlled mutation that violates the named property. Require the
   check to fail at the intended assertion with the intended reason, rather than
   during setup or for an unrelated error.
4. Restore the property and require the positive control to pass after doing
   real work. Inspect counts and typed rows so zero execution cannot pass.
5. Check bounds arithmetically: the watcher's poll count, interval, startup
   allowance, and outer timeout must permit the awaited event to occur.
6. Check metric direction and denominator. Progress toward the stated goal must
   move the metric in the direction its label implies, without dropping rows or
   moving work outside the measured population.

This catches the recurring failures seen in this project: a test asserting the
opposite of its name, an assertion passing through the wrong path, a gate
demanding a system that is disabled, a watcher whose own bound makes completion
impossible, a checker exempted as unschedulable, and a metric that gets worse in
meaning while its number improves. Treat each as a defect in the evidence until
the negative control demonstrates otherwise. Never make a red check green by
weakening an assertion, comparator, timeout, required population, or failure
classification.

## Maintain `tests/DEBUGGING.md` as current state

`tests/DEBUGGING.md` is the human-readable index of investigations that are
active now. It complements rather than copies the immutable validate LEDGER and
the per-cell metadata in `ci/compat-envelope/cells.json`. If the file does not
yet exist, create it only when there is a current misbehavior to record and add
its required `support-data` entry to
`tests/e2e/manifests/inventory/test-files.json` in the same change.

- Use one H1 heading for every current manifest `bucket` from
  `tests/e2e/manifests/*.yaml`, spelled exactly like its `bucket` field and kept
  in lexical order. H1s remain even when their bucket has no current failures;
  add or remove them when the manifest bucket set changes.
- Under the owning bucket, use one H2 heading for each test that is
  **currently** misbehaving. Use the manifest test id verbatim. If several
  mode/backend cells fail for that test, keep them in the same H2 and identify
  each complete cell separately.
- Add evidence as timestamped H3 entries. Use UTC and the exact recorded outcome
  (`PASS`, `FAIL`, `ERROR`, `HOST-INAPPLICABLE`, a typed verdict such as
  `no_result`, or `mixed PASS/FAIL` for a repeated sample); do not invent a
  replacement label. Record the full SHA and tree, cell, command and exit,
  result class, first divergent coordinates when present, contention/host facts,
  retained evidence paths, current hypothesis, what the observation ruled out,
  and the next discriminating check. Link large logs; do not paste them into the
  journal.
- The journal describes the present investigation, not its archive. Git history
  preserves removed prose; the validate LEDGER preserves run history; per-cell
  metadata preserves the latest checked-in cell evidence.

Use this shape:

```markdown
# system-utils

## system-utils/mktemp-name

### 2026-08-26T19:42:10Z — portable / verify / ptrace — FAIL

- Hermit: `<40-hex SHA>`; tree: `<40-hex tree>`; binary: `<path, binary_build_sha, sha256>`
- Command: `<literal command>`; exit: `<status>`; contention: `<jobs/load>`
- Evidence: `<validate run handle, result directory, verify report, log>`
- Observed: `<first divergent record/turn/syscall or last progress>`
- Current explanation: `<what the evidence supports, in project vocabulary>`
- Ruled out / next: `<negative result>; <next check that can distinguish causes>`
```

**Deletion is mandatory.** Remove the entire H2 as soon as every cell named in
it is green and non-flaky. Do not leave a `resolved` section, tombstone, or old
hypothesis in the live journal: stale failures read as current and are worse
than no journal. A retry that fails and then passes is evidence of flakiness,
not grounds for deletion. `cells.json` status alone is not enough. For a cell in
the current selected plan, require its typed contract to pass in a complete
exact-head validation run; a focused pass alone is not enough. For a manual or
CI-disabled cell, require an exact-head typed probe that exercises its declared
mode/backend contract; do not require promotion into ordinary validation merely
to prune the journal. If the product gap that keeps it disabled still exists,
the test is still misbehaving and its H2 stays.

For a previously flaky determinism cell, also require the same source and
binary identity to complete a fixed-tree repeat sample with no FAIL, ERROR,
`no_result`, timeout, or mixed outcomes (L4's 20 successful repetitions unless
a stricter test-specific policy applies), and clear any corresponding
measured-flaky registry entry under that registry's evidence rule. If any cell
named in the H2 remains red, flaky, unobserved, or `no_result`, keep the H2 and
update its newest timestamped entry. Do not delete the H2 until the check has
also passed the negative- and positive-control requirements above; an
untrustworthy green cannot prove that the misbehavior is gone.

## 0. First move, always

```bash
# Separate hermit's log (stderr) from the guest's own output (stdout):
target/release/hermit --log info run -- <program> [args...] 2>/tmp/h.log
#   ^ global flag, BEFORE the subcommand.   guest stdout stays on your terminal
wc -l /tmp/h.log      # a trivial `echo hello` produces ~350 INFO lines
```

Do **not** interleave hermit logs into guest stdout. Either redirect stderr as
above, or use the dedicated flag:

```bash
target/release/hermit --log info --log-file /tmp/h.log run -- <program>
```

`--log-file` (env `HERMIT_LOG_FILE`) writes the trace to a file and leaves the
guest's stdout/stderr untouched — the cleanest way to keep the two streams
apart.

### Log levels (`-l/--log`, env `HERMIT_LOG`)

| Level | Use it for |
| --- | --- |
| `error` / `warn` | Quiet; only when you want the guest to run near-normally (used for QEMU boots). |
| `info` | **Default debugging level.** Every COMMIT (scheduling turn), every DETLOG syscall, every virtual-time advance. Start here. |
| `debug` | Adds `reverie_ptrace::task` events, per-step scheduler internals, `tracee` lines. ~2-3x the volume. Use when INFO isn't enough. |
| `trace` | Everything, including `[sched-step*]` micro-steps and quiescence waits. Very large; scope it (see per-target filtering). |

Per-target filtering uses `tracing`/`RUST_LOG` syntax, so you can crank up one
module without drowning in the rest:

```bash
HERMIT_LOG='info,detcore::scheduler=trace' target/release/hermit run -- <program> 2>/tmp/h.log
```

## 1. How to read the trace

Every line is `TIMESTAMP LEVEL target: message`. The `target` tells you the
subsystem: `detcore`, `detcore::scheduler`, `detcore::scheduler::runqueue`,
`detcore::syscalls::files`, `detcore::tool_global`, `detcore::tool_local`,
`reverie_ptrace::task`. Grep by target to isolate a subsystem.

The two message classes that matter most both live in the deterministic trace:

**COMMIT lines** — one per scheduler *turn*. This is the serialized schedule.

```
[sched-step5] >>>>>>>
 COMMIT turn 0, dettid 3 using resources {ParentContinue { parent: DetPid(3), child: DetPid(3) }: W}, on previously committed 1_640_995_199.000_000_000s
 COMMIT turn 1, dettid 3 using resources {MemAddrSpace(DetPid(3)): RW}, on previously committed 1_640_995_199.000_500_000s
```

Read it as: *turn N, thread `dettid`, acquired these resources (R/W), at this
committed virtual time.* The **sequence of `(turn, dettid)` pairs is the
schedule** — the single most important thing to diff between two runs.

**DETLOG lines** — deterministic facts: syscalls, their results, RNG seeds, etc.

```
DETLOG [syscall][detcore, dtid 3] inbound syscall: openat(-100, ... "/etc/ld.so.cache", OFlag(O_CLOEXEC)) = ?
DETLOG [syscall][detcore, dtid 3] finish syscall #3: openat(...) = Ok(3)
DETLOG SCHEDRAND: seeding scheduler runqueue with seed 0
DETLOG USER RAND: seeding PRNG for root thread with seed 0
```

`inbound syscall: ... = ?` is interception; `finish syscall #N: ... = Ok(..)` is
the sanitized result handed back to the guest. A syscall that appears inbound
but whose result looks like passthrough of a host value is a determinism
suspect.

**Virtual time (DetTime / LogicalTime).** Time in hermit is *logical*, not wall
clock. `detcore-model/src/time.rs` defines:

- `LogicalTime(u64)` — absolute nanoseconds since a fixed epoch
  (`starting_micros`, default `1640995199000000` = 2021-12-31T23:59:59). This is
  why guest timestamps are identical across runs.
- `DetTime { syscalls, rcbs, nondet_instrs, starting_micros, multiplier }` —
  virtual time is a deterministic *function of work done*, not of the host
  clock. It advances by counting **syscalls**, **RCBs** (retired conditional
  branches, from the PMU — the preemption clock), and **nondet_instrs**
  (`rdtsc`/`cpuid`).

You see it advance in the trace:

```
[dtid 3] inbound rdtsc, new logical time: DetTime { syscalls: 1, rcbs: 49, nondet_instrs: 1, starting_micros: 1640995199000000, multiplier: 1.0 }
```

and summarized at shutdown:

```
Internally, the hermit scheduler ran 34 turns, recorded 0 events, replayed 0 events (0 desynced)
Final virtual global (cpu) time: 1_640_995_199.019_160_055s
```

If the RCB counts for the same logical point differ between two runs, the guest
executed a different number of branches — a real divergence, not a clock
artifact.

### Quick grep cookbook

```bash
grep ' COMMIT turn '            /tmp/h.log   # the schedule (turn, dettid) sequence
grep ' DETLOG '                 /tmp/h.log   # deterministic facts
grep 'inbound syscall'          /tmp/h.log   # syscalls intercepted, in order
grep 'new logical time'         /tmp/h.log   # virtual-time advances (DetTime)
grep -iE 'park|unpark|go-ahead|New thread|run queue|quiescen' /tmp/h.log  # thread lifecycle
grep -oE 'detcore[a-z_:]*'      /tmp/h.log | sort | uniq -c | sort -rn    # subsystem histogram
```

## 2. Finding a nondeterminism / divergence point

When `hermit run --strict --verify --verify-strict` reports
"nondeterministic", Hermit already ran twice and compared exact
exit/stdout/stderr plus INFO events under `BitwiseInfoV1`. To localize the
divergence yourself, capture two runs and use the **built-in log differ**. Its
more aggressive normalization of hex pointers, tmp paths, `/proc/<pid>/`, and
elapsed-time fields makes it a diagnostic aid only. The final fix must
produce `bitwise_parity: true` through `--verify-json`, with nonzero compared
INFO-message counts. Apply the same requirement to KVM; backend capability or
output/status repeatability alone is not evidence that a particular KVM cell
reached L2.

```bash
target/release/hermit --log info run -- <program> 2>/tmp/a.log
target/release/hermit --log info run -- <program> 2>/tmp/b.log
target/release/hermit log-diff /tmp/a.log /tmp/b.log # compares COMMIT + DETLOG only
```

Useful `log-diff` flags (`detcore/src/logdiff.rs`):

| Flag | Effect |
| --- | --- |
| `--unsafe-strip-lines` | **Non-parity diagnostic only.** Erases timestamps and syscall values; using it to make a failing parity diff pass is cheating. |
| `--syscall-history <N>` | Print the N completed syscalls *before* each divergence — the context that tells you what led up to it. |
| `--ignore-lines <substr>` | Drop lines containing a substring before comparing (repeatable). |
| `--skip-commit` / `--skip-detlog` | Compare only DETLOG, or only COMMIT, to tell a *scheduling* divergence from a *syscall/data* divergence. |
| `--include-detlogs syscall,syscallresult,other` | Narrow which DETLOG classes count. |
| `--limit 0` | Don't elide after 20 diffs; show all. |

**Interpretation:** the *first* divergence is the one that matters; everything
after it is downstream noise. If the first diff is a **COMMIT** line
(`(turn, dettid)` differs), the *schedule* diverged — a thread-interleaving
problem. If COMMITs match but a **DETLOG** line differs, the schedule is stable
but a syscall returned different data — an unvirtualized source.

## 3. Common root causes, and their log signatures

| Symptom in the log | Likely cause | Where to look |
| --- | --- | --- |
| First diff is a `COMMIT` line — different `(turn, dettid)` order between runs | **Thread-interleaving nondeterminism.** Often from `--no-sequentialize-threads`, or a futex/blocking-IO race. | `detcore/src/scheduler.rs`; check for relaxation flags. |
| DETLOG syscall result differs; value looks like a live host reading (time, meminfo, rand) | **Unvirtualized time / entropy source** falling through to the host. | `detcore/src/time.rs`, the relevant `detcore/src/syscalls/` handler. |
| `WARN`/`ERROR` "unsupported syscall" or a syscall returning `ENOSYS` unexpectedly | **Unhandled syscall falling through.** Add `--panic-on-unsupported-syscalls` to make it fatal + get a backtrace. | `detcore/src/syscalls/`. |
| `cpuid` in the trace and behavior varies by host | **CPUID leaking real hardware.** Try `--no-virtualize-cpuid` to confirm it's CPUID-related; the host may lack CPUID faulting. | `detcore/src/cpuid.rs`. |
| Run *hangs* with no forward progress; last lines are `[sched-step*]` / quiescence waits | Scheduler waiting on a wakeup that never causally pairs (e.g. FIFO open rendezvous), **or** a long syscall-free loop being precise-preemption single-stepped (slow, not hung). | `detcore/src/scheduler.rs`; try `--debug-futex-mode polling`. |
| `--verify` aborts before run 2 | Run 1 exited via a **signal** (verify needs a clean exit to compare two runs). | Use plain `--strict` x3 instead. |

## 4. Debugging-specific CLI flags

Global (before the subcommand): `--log`, `--log-file`, `--backend <ptrace|dbt|kvm>`.

On `run` (see `hermit run --help`), the internal/debug flags:

- `--panic-on-unsupported-syscalls` — turn a silent fallthrough into a fatal
  error with a backtrace (debugging detcore itself; do not use in production).
- `--stacktrace-event <index[,path]>` — print the guest stack at a given
  schedule event; pairs with record/replay.
- `--preemption-stacktrace[-log-file <f>]` — dump a stack at each preemption
  (chaos mode).
- `--debug-futex-mode <precise|polling|external>` — switch the futex model when
  diagnosing a futex-related hang.
- `--debug-externalize-sockets` — treat all sockets as external/nondeterministic
  to isolate socket-driven nondeterminism.
- `--detlog-heap` / `--detlog-stack` — log hashes of heap/stack maps for
  memory-determinism (L3) checking.
- `--stop-after-turn <N>` / `--stop-after-iter <N>` — halt after a scheduler
  turn/loop iteration (requires `--sequentialize-threads`) to bisect a schedule.
- `--imprecise-timers` / RCB-count knobs — change how logical time is derived
  when the PMU is unavailable or noisy.
- `--gdbserver` — start a gdbserver for remote debugging.

Higher-level analysis subcommands: `hermit log-diff` (above),
`hermit analyze` (analyze passing vs failing runs), and `hermit bisect`
(`--good <schedule> --bad <schedule>` to localize a race between two recorded
schedules).

## 5. Assurance ladder (name the level you reached)

Per `AGENTS.md`, never say "works". State the level, backend, log level, and
relaxations:

- **L1** deterministic: `hermit run --strict` completes.
- **L2** canonical full-observation parity:
  `hermit run --strict --verify --verify-strict --verify-json <path> -- ...`,
  with JSON `bitwise_parity: true` and nonzero compared INFO-message counts.
- **L3** memory determinism: add `--detlog-heap --detlog-stack` to L2.
- **L4** stress-hardened: L2/L3 repeated 20 times with no divergence.

Example of a correct report: "passes at L2 (ptrace backend, `--log` default,
relaxations: none)".

## 6. Source-code map (read *after* the log points you here)

- `detcore/src/scheduler.rs` — the sched loop; `[scheduler]`, `[sched-step*]`,
  and COMMIT emission (`info!`/`debug!`/`trace!`). The COMMIT point is step 4.
- `detcore/src/logdiff.rs` — the log comparator: `strip_log_entry`
  normalization, `is_commit`/`is_detlog`, `LogComparisonMode`, `LogDiffOpts`.
- `detcore-model/src/time.rs` — `LogicalTime`, `DetTime`, `GlobalTime`, and the
  RCB↔nanosecond conversions.
- `detcore/src/syscalls/` — per-syscall handlers (`files.rs`, etc.).
- `detcore/src/cpuid.rs`, `detcore/src/time.rs` — CPUID and time virtualization.
- `detcore/src/tool_local.rs` / `tool_global.rs` — per-task events vs shared
  deterministic state (they talk over RPC).
- `docs/Developers/Architecture.md` — architecture overview.
