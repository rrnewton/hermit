# Hermit demos walkthrough

The `demos/` suite moves from deterministic user processes to deterministic
whole-system Linux execution. Demos 1-4 introduce repeatable runs, replay,
seeded concurrency exploration, and schedule bisection. Demo 5 boots Linux once
and stores a live QEMU snapshot. Demos 6 and 7 reuse that snapshot, so later
experiments do not pay the boot cost again.

Demo 7 is the kernel-introspection finale: drgn reads the restored guest task
list without executing guest instructions, Hermit advances the guest through a
fixed interval, and drgn reads the changed list. Two independent restores must
produce exactly the same before list, after list, and diff.

Demo 8 returns from the whole-system track to a single multithreaded userspace
program and closes the loop on chaos scheduling: it takes a real concurrency
bug that blind execution almost never hits, surfaces it deterministically on a
recorded seed, and replays the crash bit-for-bit.

## Dependency-aware entry points

Run demos through the repository Makefile when starting from a fresh clone:

```bash
make demo1                 # any individual checked-in demo, demo1 .. demo8
make demo6 DEMO6_COMMAND='ls /'
make demo7
make demo8                 # skips cleanly if its prebuilt assets are absent
make demos                 # every checked-in demo in order
```

The targets model artifact dependencies rather than requiring manual ordering:

```text
process prerequisites ──> demos 1, 2, 3, 4
                          └──> demo 8 (+ prebuilt ASAN btrfs-convert assets)
process + QEMU prerequisites ──> demo 5 ──> hermit-boot.qcow2
                                            ├──> demo 6
                                            └──> demo 7 (+ drgn/readelf)
```

Demos 1-4 already build their shared Hermit binaries incrementally. Demo 5
provisions the fixed kernel/initramfs and atomically publishes
`ignored/qemu-linux/hermit-boot.qcow2`. That file is a real Make prerequisite
of demos 6 and 7. If it is present, both restore it without booting Linux. If it
is missing, `make demo6`, `make demo7`, and the direct demo 6/7 scripts invoke
Demo 5 once and continue with the newly stored snapshot. A deliberately chosen
custom snapshot path is never overwritten implicitly; a missing custom path
fails with a specific remediation message.

The unified directory contains demos 1-8; every checked-in demo has a
dependency-aware target. Demo 8 joins the graph on the process prerequisites
plus its prebuilt ASAN assets rather than an artificial serial chain, and skips
cleanly when those assets are absent.

## Prepare the suite

From the repository root, initialize the pinned submodules, install the core
and QEMU prerequisites described in `demos/README.md`, and build Hermit:

```bash
git submodule update --init --recursive
make install-deps-core
make
```

Demos 5-7 require Python 3, QEMU, `qemu-img`, static BusyBox, and the QEMU asset
tools. Demo 7 additionally requires `drgn` and `readelf`. Check the QEMU side
without building or downloading anything:

```bash
./demos/lib/qemu-assets.sh --check
```

Use the same `QEMU_BIN` setting for Demos 5, 6, and 7. A live QEMU snapshot
contains machine-version state and must be restored by a compatible QEMU.

## Demos 1-4: process determinism

Run the process-level demonstrations separately so each result is easy to
inspect:

```bash
./demos/01-deterministic-run.sh
./demos/02-record-replay.sh
./demos/03-chaos-concurrency.sh
./demos/04-schedule-bisection.sh
```

- Demo 1 repeats a racy process under Hermit and verifies deterministic output.
- Demo 2 records a run and replays it to completion.
- Demo 3 uses stable seeds to reproduce selected concurrent schedules.
- Demo 4 bisects passing and failing schedules to isolate the decisive order.

Demo 4 is intentionally the slowest process-level example.

## Demo 5: boot once and store the Linux snapshot

Run Demo 5 explicitly when you want to prepare the shared snapshot ahead of
time:

```bash
make demo5
```

Hermit runs QEMU with strict checking and deterministic instruction-derived
time. The controller waits for the initramfs shell, asks QEMU to save the live
machine under the internal name `hermit-boot`, and atomically publishes the
reusable image here:

```text
ignored/qemu-linux/hermit-boot.qcow2
```

This is the only Linux boot needed for the remaining walkthrough.

## Demo 6: resume and run a command

Demo 6 copies the stored image, restores `hermit-boot`, and runs one command at
the already-booted shell:

```bash
./demos/06-qemu-resume.py 'ls /'
```

Repeating the same command compares the deterministic guest output and saved
post-command machine state. Different commands get independent evidence.

## Demo 7: read, advance, and read again

Run the drgn evolution check:

```bash
make demo7
```

One invocation performs two complete evolution runs. Each run does the
following:

1. Copy and restore the Demo 5 image with QEMU's virtual CPUs stopped.
2. Wait for QEMU to enter a Hermit ptrace stop, then stop Hermit's exact tracer
   thread group.
3. Read every guest task row through drgn.
4. Queue a fixed shell action, resume the tracer and QEMU, create two persistent
   `sleep` tasks, and wait for a 1000-microsecond guest-virtual timer.
5. Stop QEMU at the action's serial marker and freeze the exact tracer again.
6. Read every task row again and compute the exact removed and added rows.

The second evolution starts from a fresh copy of the same stored image. Demo 7
fails unless both complete before lists, both complete after lists, and both
diffs are identical. The display shows a short prefix, but the comparison uses
every task.

Expected output includes this stable core:

```text
evolution 1: before_tasks=84 after_tasks=86 removed=0 added=2 read_states=t/T,t/T serial_delta=0/0
evolution 2: before_tasks=84 after_tasks=86 removed=0 added=2 read_states=t/T,t/T serial_delta=0/0
before tasks (84 total; first 16 shown, pid comm):
      0 swapper/0
      1 sh
      2 kthreadd
      3 pool_workqueue_
      4 kworker/R-rcu_g
      5 kworker/R-sync_
      6 kworker/R-kvfre
      7 kworker/R-slub_
      8 kworker/R-netns
      9 kworker/0:0
     10 kworker/0:0H
     11 kworker/0:1
     12 kworker/u4:0
     13 kworker/R-mm_pe
     14 kworker/u4:1
     15 ksoftirqd/0
after tasks (86 total; first 16 shown, pid comm):
      0 swapper/0
      1 sh
      2 kthreadd
      3 pool_workqueue_
      4 kworker/R-rcu_g
      5 kworker/R-sync_
      6 kworker/R-kvfre
      7 kworker/R-slub_
      8 kworker/R-netns
      9 kworker/0:0
     10 kworker/0:0H
     11 kworker/0:1
     12 kworker/u4:0
     13 kworker/R-mm_pe
     14 kworker/u4:1
     15 ksoftirqd/0
task-list diff (- before, + after):
  +    91 sleep
  +    92 sleep
RESULT: restored phase-5 snapshot; fixed_virtual_advance_us=1000; task_lists_differ=yes; evolution_reproducible=yes; read_virtual_time_advanced=no
```

The exact kernel-worker prefix belongs to the fixed demo kernel and stored
snapshot. The important assertions are that the complete lists reproduce, the
same two task rows are added, and both read intervals remain quiescent.

## Why the drgn reads have zero guest-time cost

drgn does not attach to QEMU. Demo 7 opens `/proc/<qemu-pid>/mem` read-only and
registers QEMU's RAM mapping as a physical-memory callback. Before exposing the
drgn program, it verifies that QEMU is in ptrace-stop state `t` and Hermit's
actual tracer thread group is stopped in state `T`.

Hermit's tracer is the only process that can let the tracee execute. While it
is stopped, drgn's host-side `pread()` calls cannot execute a QEMU or guest
instruction. Each read also requires zero serial-byte growth. Consequently the
two observation windows advance guest virtual time by zero.

The interval between those windows deliberately advances the guest. QEMU uses
instruction-derived virtual time, and the fixed action waits on a one-
millisecond guest timer after creating the two processes. This separates the
zero-Heisenberg observations from the deterministic state transition they
measure.

## Useful controls

- `DEMO07_TASK_LIMIT` changes only how many leading task rows are printed; all
  rows remain part of the comparison.
- `DEMO07_RUNS` may request more than two independent restores, but values below
  two are rejected because they cannot demonstrate reproduction.
- `DEMO07_SNAPSHOT_DISK` and `DEMO07_SNAPSHOT_NAME` select the Demo 5 artifact.
- `DEMO07_VMLINUX` selects matching kernel debug information. Demo 7 verifies
  its GNU BuildID against VMCOREINFO before reading kernel objects.
- `QEMU_ASSETS`, `QEMU_BIN`, and `HERMIT_RELEASE` select the shared assets and
  executables used across the QEMU demos.

On hosts that require the forward proxy for first-use public downloads, prefix
the asset-producing commands with `with-proxy`. Restores of already-cached
assets do not require network access.

## Demo 8: find and replay a schedule-dependent use-after-free

Demo 8 uses a real multithreaded userspace program to show the payoff of
deterministic chaos scheduling. `btrfs-convert` (btrfs-progs) runs a background
progress thread that dereferences a shared `struct task_info *info` while the
main thread copies inodes. Before upstream commit `73e211a7` the teardown path
detached that thread and never joined it, so `task_deinit()` could `free(info)`
while the progress thread was still reading it — a heap use-after-free whose
occurrence depends entirely on the teardown interleaving. That is exactly the
kind of rare, order-sensitive bug chaos scheduling is built to expose.

```bash
make demo8                       # or: ./demos/08-btrfs-convert-uaf.sh
```

Two prebuilt AddressSanitizer binaries (`buggy` = pre-73e211a7, `fixed` =
73e211a7) and a populated ext4 image live under `ignored/demo08-btrfs/`. ASAN
turns the latent UAF into an observable abort with a precise report. When the
assets are absent the demo prints `SKIPPED` and exits 0, so `make all` stays
green; `demos/08-btrfs-convert-uaf.md` has the build recipe.

### Blind execution almost never hits it

Native execution misses the bug because the teardown window is tiny on real
hardware. The experiment behind this demo swept the buggy binary 40 times
natively and across 32 chaos seeds:

| Execution | Runs | UAF crashes |
| --- | --- | --- |
| native (blind) buggy | 40 | **0** — dormant |
| hermit `--chaos` buggy, seeds 0-31 | 32 | **2** (seeds **15**, **19**) |
| hermit `--chaos` fixed, seeds 0-31 | 32 | **0** — the fix closes the window |

(from `experiments/btrfs-convert-progress-uaf-chaos_20260729/results.csv`.)

The demo then drives one known crashing seed end to end. Step 1 confirms the
blind baseline — the same buggy binary, run natively, exits cleanly:

```text
=== Demo 08: schedule-dependent btrfs-convert progress-thread UAF ===
seed=15 timeout=90s
...
--- Step 1: native buggy btrfs-convert (blind execution) ---
native buggy: clean exit (UAF dormant, as expected)
```

### Chaos finds the crash on the recorded seed

With `--sched-seed 15` the chaos scheduler lands the main thread's `free` before
the progress thread's post-wake dereference, and ASAN aborts with the textbook
73e211a7 signature. Frame `#0` is the progress thread's read of `*info` in
`task_period_wait`; frame `#1` is its caller `print_copied_inodes` — the load
happens *after* `task_deinit` freed `info`:

```text
--- Step 2: chaos buggy, --sched-seed 15 (expect ASAN UAF) ---
==3==ERROR: AddressSanitizer: heap-use-after-free on address 0x606000000330 at pc 0x00000052793b bp 0x7ffff3ffeaf0 sp 0x7ffff3ffeae0
    #0 0x52793a in task_period_wait common/task-utils.c:154
    #1 0x41218a in print_copied_inodes convert/main.c:170
SUMMARY: AddressSanitizer: heap-use-after-free common/task-utils.c:154 in task_period_wait
chaos buggy: reproduced the use-after-free
```

`--sched-seed 15` is the recorded seed: it names the exact interleaving, so the
crash is reproducible rather than a lucky one-off.

### The fix closes the window on the same seed

The differential runs the `fixed` binary under the *same* seed and schedule.
The no-detach + join teardown removes the race, so it exits cleanly:

```text
--- Step 3: chaos fixed, --sched-seed 15 (expect clean) ---
chaos fixed: clean exit on the crashing seed (73e211a7 closes the window)
```

### The crash replays bit-for-bit

Re-running the crashing seed with the identical image path (hermit determinism
is per-input, and the faulting heap address depends on `argv`) reproduces a
byte-identical guest ASAN report — same heap address, PC, and frames:

```text
--- Step 4: replay --sched-seed 15, confirm identical crash ---
replay: guest ASAN report byte-identical (same heap address, PC, frames)

=== Demo 08: SUCCESS ===
native missed the UAF; chaos found it on seed 15, the fix closed
it, and the crash replayed deterministically.
```

### Observability adaptation (stated plainly)

The historical code paces the progress thread with a wall-clock
`CLOCK_MONOTONIC` timerfd. Hermit virtualizes `CLOCK_MONOTONIC` to logical
(instruction-derived) time, which barely advances during this I/O-bound
conversion, so the timer never fires and the race stays dormant even under
chaos. Both demo binaries therefore replace the timerfd with a pipe woken by a
single teardown byte — applied **identically** to the buggy and fixed variants,
so the only behavioral difference between them remains the real detach/no-join
bug. ASAN options are baked into the binaries because the host `ASAN_OPTIONS`
does not reach the hermit guest. Neither change alters which teardown ordering
is safe; they only make the existing race reachable and observable under
hermit's logical clock. Full detail: `demos/08-btrfs-convert-uaf.md`.

Useful overrides: `DEMO08_CRASH_SEED` (default 15), `DEMO08_TIMEOUT` (default
90), `DEMO08_DIR`, `DEMO08_ARTIFACTS`, and `HERMIT_RELEASE`.
