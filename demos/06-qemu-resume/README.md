# Demo 6: Resume the Linux snapshot and run a command

Booting Linux under Hermit took 75.7 seconds for one `make -C demos demo5` on
a shared 316-CPU host (2026-09-30, Hermit 0.2.0 gdc92644f96f4, QEMU 10.1.2; see
demo 5). On the same host, restoring the saved machine and running one command
took 15.7 to 16.5 seconds of Hermit/QEMU time over eleven resumes, or 10.0 to
11.4 seconds over three resumes that did not save a post-command snapshot. One
`make -C demos demo6` took 25.3 seconds (demo 6 took 26 seconds inside a
`make -C demos group3`), and four first invocations of a command, which resume
twice, took 46.0 to 46.4 seconds. This demo restores the snapshot that
[demo 5](../05-qemu-boot/README.md) saved, has the already-running guest shell
run one command, prints the command's output, and saves the machine again after
the command. Because the resumed machine runs under Hermit, running the same
command again produces identical guest output and a byte-identical
post-command snapshot.

## Prerequisites

- Everything [demo 5](../05-qemu-boot/README.md#prerequisites) needs.
- The boot snapshot from demo 5. If it is missing, this demo runs demo 5 first.

The resume itself does not use the hardware performance counters (see "How it
works"), but demo 5 does if it has to run first.

## Run it

From the repository root:

```bash
demos/06-qemu-resume/run.py
```

With no argument the guest runs `uname -a`. Pass any single-line shell command
to run it inside the guest instead:

```bash
demos/06-qemu-resume/run.py 'ls /'
demos/06-qemu-resume/run.py 'cat /proc/meminfo | head -n 3'
```

`make -C demos demo6` runs the default command, and
`make -C demos demo6 DEMO6_COMMAND='ls /'` runs another simple one. Quote a
command that contains shell syntax such as `|` and pass it to `run.py`
directly, because Make would otherwise hand the `|` to your own shell.

Each distinct command has its own reference run. On the first run of a command,
one invocation resumes twice: the first resume is saved as that command's
reference and the second is compared with it. Set `QEMU_RESUME_REPEAT=0` to
resume only once.

## What you will see

Captured on 2026-09-30: the second resume of a first `uname -a` invocation,
with the tail of Hermit's log cut (marked `...`).

```text
================================================================================
=====                     Demo 6: QEMU Snapshot Resume                     =====

QEMU restores the live shell, runs one command, saves a post-command
snapshot by default, and compares repeats keyed by the command string.
Dependency check passed: hermit 0.2.0 (...) (...)

================================================================================

=== Resume hermit-boot and run: uname -a ===
Restoring snapshot (timeout: 120s)...
Hermit/QEMU resume: done (16.3s)
TIMER_DONE label=Hermit/QEMU resume elapsed=16.3s

=== Guest serial output ===
Linux (none) 6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79 #1 SMP Thu Dec 18 12:27:13 PST 2025 x86_64 x86_64 x86_64 GNU/Linux

=== Hermit INFO tail (wall-clock timestamps stripped) ===
...
  ------------------------------ hermit run report ------------------------------
Final thread-tree was: [3 [5 7 9 11 13 15 17 19]]
There were 2 group leaders of 9 thread(s) total.
Internally, the hermit scheduler ran 288022 turns, recorded 0 events (0 desynced)
Final virtual global (cpu) time: 1_767_226_582.454_550_000s
Elapsed virtual global (cpu) time: 982.454_550_000s
Timeslice stats: min=0ns max=125000000ns mean=4006895ns count=211951

=== Automatic repeat verification ===
PASS: QEMU argv matches first run
PASS: QEMU version matches (QEMU emulator version 10.1.2 (...))
PASS: QEMU binary SHA-256 matches (...)
PASS: qcow2 SHA-256 matches (9f2cc4675b3e641386c358d8ef8234e100ec829ad4de63577fb3e98cf8a79c66)
PASS: guest output SHA-256 matches (42a2dfb7b01dab06b2fc0b36483d3f75aaa7d33d239a636b3ae2daf9b0f81f06)
PASS: Hermit INFO log matches first run exactly apart from the wall-clock prefix (Hermit-marked host addresses compared by first appearance)
PASS: all repeat checks match the first run

DETERMINISTIC: snapshot SHA-256 matches the previous run:
   9f2cc4675b3e641386c358d8ef8234e100ec829ad4de63577fb3e98cf8a79c66
   Resume is bitwise-reproducible under Hermit.
Post-command snapshot: ignored/qemu-linux/resume-metadata/28ba533b0f3c4df63d6b4a5ead73860697bdf735bb353e4ca928474889eb8a15/run-history/resume-...Z-.../post-command.qcow2
Post-command SHA-256: 9f2cc4675b3e641386c358d8ef8234e100ec829ad4de63577fb3e98cf8a79c66
Run metadata: ignored/qemu-linux/resume-metadata/28ba533b0f3c4df63d6b4a5ead73860697bdf735bb353e4ca928474889eb8a15/run-history/resume-...Z-.../run-metadata.json

=== Demo 6: QEMU Snapshot Resume: SUCCESS ===
```

Elided (`...`): your Hermit version and path, the tail of Hermit's log, the
QEMU package name and binary hash, and the per-run directory names (a
timestamp and a process ID). The wall-clock times (`16.3s`) depend on host
load; in a terminal the `Hermit/QEMU resume` counter updates in place until it
reads `done`. The `PASS: Hermit INFO log` line is shown in the current
comparator's wording; the capture printed an earlier comparator's wording. Its
logs pass the current comparison too: they differed in no line once the
wall-clock prefix was removed (see below). The directory named `28ba533b...`
is the SHA-256 of the command string `uname -a`. Your hashes and virtual times
will match these only with the same QEMU build, kernel and demo 5 snapshot. As
in demo 5, a saved reference run no longer applies after you rebuild Hermit,
change QEMU or the kernel, replace demo 5's snapshot, edit
`demos/lib/demo_common.py` or `demos/lib/qemu_controller.py`, or run the demo
with a different Python interpreter: the interpreter that runs `run.py` is also
the guest's controller program. Run `demos/clean.sh` to start over.

On 2026-09-30 six `uname -a` resumes ran: two in a first invocation in this
checkout, one `make -C demos demo6`, one `make -C demos group3`, and two in a
first invocation from a copy of the `demos` directory under `/var/tmp`. In all
six the post-command snapshot, the guest output, the scheduler's 288022 turns,
the virtual times, and the whole Hermit log (258,506,138 bytes, 1,604,193
lines) were identical;
the logs differed in no line once each line's leading wall-clock timestamp was
removed. The other two commands behaved the same way: `ls /` (three resumes)
gave post-command snapshot `146f380f...` and guest output `81acdc95...`, and
`cat /proc/meminfo | head -n 3` (two resumes) gave `642d32db...` and
`a78a631c...`.

The first resume of a new command ends instead with:

```text
=== Automatic repeat verification ===
Saved this run as the reference run for this command at ignored/qemu-linux/resume-metadata/28ba533b0f3c4df63d6b4a5ead73860697bdf735bb353e4ca928474889eb8a15/run-metadata.json
Post-command snapshot: ignored/qemu-linux/resume-metadata/28ba533b0f3c4df63d6b4a5ead73860697bdf735bb353e4ca928474889eb8a15/run-history/resume-...Z-.../post-command.qcow2
Post-command SHA-256: 9f2cc4675b3e641386c358d8ef8234e100ec829ad4de63577fb3e98cf8a79c66
Run metadata: ignored/qemu-linux/resume-metadata/28ba533b0f3c4df63d6b4a5ead73860697bdf735bb353e4ca928474889eb8a15/run-history/resume-...Z-.../run-metadata.json

=== Demo 6: QEMU Snapshot Resume: FIRST RUN SAVED ===

=== Resume again and compare with the reference run just saved ===
```

## What to notice

- The guest did not boot. The kernel messages from demo 5 do not appear; the
  guest continues from the exact moment the snapshot was taken and runs the
  command straight away.
- Where demo 5's repeat check has a `serial output SHA-256` line, this one has
  `guest output SHA-256`: the output of your command, cut out of the serial
  transcript from between the command markers. The demo does not hash the rest
  of the transcript; we compared each run's archived `serial.log` by hand, and
  it was byte-identical across the repeats of each command above.
- The reference run is stored per command (under a directory named after the
  command's SHA-256), so `uname -a` and `ls /` are checked independently.
- As in demo 5, one value in the Hermit log still depends on how `hermit` was
  started. QEMU reads `/proc/self/status`, and Hermit passes that file's
  `SigIgn` line, the set of signals the process ignores, through from the host
  unchanged: https://github.com/rrnewton/hermit/issues/3441. On 2026-09-30, in
  two invocations from a fresh clone, the only failed comparison was the second
  resume of the first invocation: QEMU's two reads of `/proc/self/status`
  differed in the bit for signal 33, while the post-command snapshot and the
  guest output matched. The second resume came after the demo's first thread,
  and glibc changes signal 33 when a program creates its first thread;
  [demo 5's "How it works"](../05-qemu-boot/README.md#how-it-works) explains
  this. The demo now starts and waits for one thread before its first resume,
  so every resume sees signal 33 in the same state. Other signals that a
  launcher leaves ignored still pass through to the guest. `nohup`, for
  example, ignores `SIGHUP`, and GNU `make` also leaves signal 32, glibc's other
  internal signal, ignored. glibc resets signal 32 only when a program cancels
  a thread, which the demo never does, so on a host where `python3` is a plain
  binary, `make -C demos demo6` and `demos/06-qemu-resume/run.py` from a shell
  can still start `hermit` with different values. So start the reference run
  and the repeats the
  same way (for example, always `make -C demos demo6`, or always
  `demos/06-qemu-resume/run.py` from a shell), and run `demos/clean.sh` when you
  switch. The comparison does not ignore this field. If a `PARTIAL` run's only
  `WARN:` is the Hermit log, check whether the differing lines are QEMU's reads
  of `/proc/self/status`.
- `--no-save-snapshot` skips saving the post-command snapshot; the guest output
  and the Hermit log are still compared, and the `qcow2 SHA-256` and
  `DETERMINISTIC:` lines do not appear. Such a run keeps its own reference, in
  the command's directory name with `-no-save-snapshot` appended, because it
  executes differently (194491 scheduler turns for `uname -a` instead of
  288022, a 169,751,813-byte Hermit log, identical in three resumes). For
  example:

  ```bash
  demos/06-qemu-resume/run.py --no-save-snapshot 'uname -a'
  ```

## How it works

The demo copies the boot snapshot and starts QEMU with the same machine
configuration as demo 5, plus an option to load the `hermit-boot` snapshot at
start-up. The command travels to the guest on a small raw disk, `/dev/vda`,
that demo 5 attached (holding `WAIT`) before it saved the snapshot. Before QEMU
starts, the demo writes the command into that disk image. When the guest
resumes, its `/init` loop reads the disk, finds a command instead of `WAIT`,
and runs it between the `__HERMIT_COMMAND_BEGIN__` and `__HERMIT_COMMAND_END__`
markers. The command is on disk before the guest runs, so nothing depends on
when the host sends it. The controller ([`lib/qemu_controller.py`](../lib/qemu_controller.py))
waits for the end marker and asks QEMU to save a snapshot named
`command-<first 16 hex digits of the command's SHA-256>`.

The resume runs under `hermit run --strict --epoch 2026-01-01T00:00:00Z
--no-rcb-time --target-timeslice 100000 --max-timeslice disabled`. It switches
QEMU's threads only at system calls and advances virtual time without counting
branches, so it needs no hardware performance counters. As in demo 5,
`--epoch` fixes the date the virtual clocks start from, `--base-env=minimal`
(plus `PYTHONDONTWRITEBYTECODE`) replaces your shell's environment, and the
guest reads `/dev/null` and writes to a pipe that the demo copies into the
Hermit log, so neither the host clock, your environment, nor the log file's
growing size reaches the guest. The controller runs from
`/tmp/hermit-demo-controller`, a copy of its source files made for each run,
and the assets directory (snapshot, command disk, serial transcript) is bound
at `/tmp/hermit-demo-assets`, so the checkout's location does not reach the
guest's command lines either. Before these paths were fixed, a fresh clone in
another directory resumed to a different post-command snapshot after 288657
scheduler turns instead of 287149.

Controls (environment variables):

| Variable | Default | Meaning |
| --- | --- | --- |
| `QEMU_RESUME_REPEAT` | `1` | Set to `0` to skip the second resume of a new command. |
| `QEMU_TIMEOUT` | `120` | Seconds before the resume is stopped. |
| `QEMU_ASSETS` | `ignored/qemu-linux` | Where demo 5's snapshot and this demo's results are kept. |
| `QEMU_BOOT_SNAPSHOT_DISK` | `$QEMU_ASSETS/hermit-boot.qcow2` | The boot snapshot to restore. |
| `QEMU_BIN` | `qemu-system-x86_64` on `PATH` | The QEMU binary. It must be the one demo 5 used. |
| `QEMU_MAX_LOG_BYTES` | 512 MiB | Stop the run if Hermit's event log grows past this size. Healthy resumes on 2026-09-30 wrote 169,751,813 bytes (`uname -a` with `--no-save-snapshot`, 10.0 to 11.4 seconds) and 258,227,125 to 259,021,561 bytes (the three commands above, saving a snapshot, 15.7 to 16.5 seconds) (Hermit 0.2.0 gdc92644f96f4, QEMU 10.1.2); Hermit 0.2.0 g770b95c505fa wrote 80 to 153 MB. |
