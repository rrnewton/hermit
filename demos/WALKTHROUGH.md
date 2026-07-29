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

Run Demo 5 once before either snapshot consumer:

```bash
./demos/05-qemu-boot.py
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
./demos/07-drgn-kernel.sh
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
