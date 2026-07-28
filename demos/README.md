# Hermit Demo Walkthrough

This walkthrough demonstrates reproducible Linux execution with Hermit from the
`dev-hermit` workspace. Hermit runs unmodified x86-64 Linux programs under the
[Reverie](https://github.com/facebookexperimental/reverie) ptrace backend and
controls common sources of nondeterminism, including thread scheduling, time,
random data, CPUID results, address layout, and selected file metadata.

The demo materials live entirely in this parent repository. The pinned
`hermit/` submodule is unmodified: the outer workspace only adds to it. The
walkthrough covers six working workflows:

1. repeat an execution with stable guest-visible inputs;
2. record an execution and replay it, with or without GDB;
3. search seeded thread schedules for a concurrency failure;
4. bisect two schedules to identify the events that change the outcome;
5. boot Linux in QEMU and save a live snapshot under Hermit's strict profile;
6. resume that snapshot and inject repeatable commands over its serial port.

> [!WARNING]
>
> Hermit is not a security boundary, and it does not make changing files or
> external network responses deterministic. Record/replay support is
> experimental and narrower than `hermit run` compatibility.

## Requirements

Use an x86-64 Linux host with Rust nightly (selected by the submodule's
`rust-toolchain.toml`), libunwind and LZMA development libraries, Linux
user/PID namespaces, and parent-child ptrace and seccomp support. GDB is needed
for the record/replay section, and the Python demo uses `/usr/bin/python3`. The
`--verify` step in demo 1 needs user-accessible CPU performance counters (PMU).
Demo 4 defaults to syscall-boundary schedule exploration; set
`ANALYZE_PREEMPTION_TIMEOUT=400000` to add precise PMU preemption.

Install the native build dependencies on Debian/Ubuntu or
CentOS/RHEL/Fedora:

```bash
make install-deps
```

`install-deps` warns before invoking `sudo` and installs `libunwind-dev`,
`liblzma-dev`, and `pkg-config` on Debian-family hosts, or
`libunwind-devel`, `xz-devel`, and `pkgconf` on Red Hat-family
hosts. In particular, Hermit's `unwind-sys` build requires the
`libunwind-ptrace.pc` file; the Makefile reports that missing dependency before
Cargo starts and points back to `make install-deps`. Running `make` initializes
the `hermit/` submodule when needed and builds the release Hermit binary.

The demos use private temporary and ignored build-artifact directories. Demos 5
and 6 additionally need `qemu-system-x86_64`, `qemu-img`, the Meta `manifold`
CLI, Ncat (`nc`), static BusyBox, `cpio`, and `gzip`. Demo 5 downloads a fixed
kernel from Manifold, verifies its SHA-256, and caches it with the generated
initramfs and qcow2 snapshot disk under `ignored/qemu-linux`.

## Layout

```text
Makefile                    # dependency install/check and release build
demos/
  README.md                 # this walkthrough
  common.sh                 # checks dependencies, builds hermit/, defines helpers
  01-deterministic-run.sh   # stable inputs, --verify
  02-record-replay.sh       # record, list, replay, replay under GDB
  03-chaos-concurrency.sh   # seeded schedules, save/replay a failing schedule
  04-schedule-bisection.sh  # portable syscall-boundary hermit analyze
  05-qemu-boot.py           # boot, snapshot, metadata, repeat verification
  06-qemu-resume.py         # resume, command snapshot, repeat verification
  lib/
    demo_common.py          # hashes, metadata, QMP, serial, strict log diff
    qemu_controller.py      # deterministic in-Hermit QEMU serial/QMP controller
    qemu-assets.sh          # internal first-run kernel/initramfs helper
    qemu-snapshot.sh        # QMP, snapshot, and stable-log helpers
```

Demos 1-4 source `demos/common.sh`, which locates the `hermit/` submodule,
builds the release and debug binaries, and defines the shared `run_hermit`,
`verify_hermit`, and `chaos_run` wrappers. The Python QEMU demos use
`demos/lib/demo_common.py` for QMP, serial streaming, hashes, metadata, and
repeat comparison. `run_hermit` and `chaos_run`
deliberately disable CPUID virtualization and PMU timer preemption so the short
examples also run on hosts without those features; CPUID is therefore a host
input in those commands, and CPU-bound guests receive fewer preemption
opportunities. `verify_hermit` is different: it keeps PMU-based preemption on
(the racy verify guest is only reliably determinized with real preemption) and
raises the log level to `info` (at `--log=error` the execution log that
`--verify` compares is empty). The demo-1 `--verify` step requires the PMU;
demo 4 only requires it when `ANALYZE_PREEMPTION_TIMEOUT` enables preemption.

## Quick Start

Clone the demo branch, install dependencies once per host, and build Hermit:

```bash
git clone https://github.com/rrnewton/dev-hermit.git
cd dev-hermit
git checkout demo
make install-deps
make
```

Run each demo individually so its output and result remain easy to inspect:

```bash
./demos/01-deterministic-run.sh
./demos/02-record-replay.sh
./demos/03-chaos-concurrency.sh
./demos/04-schedule-bisection.sh
./demos/05-qemu-boot.py
./demos/06-qemu-resume.py 'ls /'
```

Demo 4 is intentionally slow. Demo 5 must complete before Demo 6 because it
creates the baseline QEMU snapshot.

Set `DEMO_SKIP_BUILD=1` to reuse an existing `hermit/target` build, or export
`HERMIT`, `HELLO_RACE`, and `HEAP_PTRS` to point at prebuilt binaries.

## What Each Demo Shows

### 1. Deterministic Run

Hermit preserves the guest exit status and output while making random bytes,
wall-clock time, Python hash seeding, and heap address layout stable across
runs. It then determinizes `examples/race.sh` -- two shells whose output
interleaves differently on every native run -- and `verify_hermit` runs it twice
under Hermit, comparing exit status, output, and thousands of DETLOG/scheduler
messages in the deterministic execution log. The guest must be idempotent: a
first run that changes a file, database, cache, or external service can
legitimately change the second run.

### 2. Record And Replay

Hermit records an execution into an isolated data directory, lists the recording
in text and JSON, and replays it to completion with `--autopilot`. It can also
record and immediately verify a replay. Without `--autopilot`, `hermit replay`
starts a replay gdbserver and GDB client; the demo drives a noninteractive GDB
session that continues the guest to completion. Keep the recording directory,
executable, inputs, and Hermit revision unchanged between recording and replay.

### 3. Chaos Concurrency Testing

`hello_race` contains an intentional data race. Chaos mode makes scheduler
choices with a seeded PRNG, so different seeds explore different interleavings
and the same seed reproduces the same result. Seed 1 passes; seed 0 reaches the
antagonistic schedule and returns the guest's expected failure status. The demo
surveys seeds 0-15, then records a failing schedule to an artifact and replays
that exact schedule, confirming the outputs match.

### 4. Schedule Bisection

`hermit analyze` first finds passing and failing schedules, then bisects their
event streams to identify the ordering that changes the outcome. It builds a
debug guest so the report can resolve source locations. This is intentionally
the slow finale: it runs the guest many times and can emit convergence
diagnostics. A successful run ends
with `Completed analysis successfully`. On the verified host, the report
identified two adjacent syscall events in different `hello_race` threads whose
order changes the outcome. Event numbers can vary with the binary and Hermit
revision. The default uses portable syscall-boundary chaos. Set
`ANALYZE_PREEMPTION_TIMEOUT=400000` to add precise PMU preemption and obtain
finer-grained source localization on a validated host.

### 5. QEMU Linux Snapshot

Hermit runs `qemu-system-x86_64`, which boots a real x86-64 Linux kernel under
TCG and reaches the initramfs serial shell. The guest RTC is checked against
Hermit's fixed 2022 virtual-time epoch rather than host wall time. Here RTC
means the guest's Real-Time Clock; the demo explicitly starts it at
`2022-01-01T00:00:00` on QEMU's VM clock.

At the shell, Demo 5 asks QEMU's QMP control socket to run `savevm hermit-boot`.
The resulting internal snapshot is stored in the ignored
`hermit-snapshot.qcow2` disk. The disk is deliberately not attached to the
guest; it exists only as QEMU's VM-state store. Demo 5 then exits QEMU over QMP
and records the image hash, raw INFO log, Hermit and QEMU versions, QEMU binary
SHA-256, and timestamp. The first run becomes
`ignored/qemu-linux/run-metadata.json`; later runs compare their exact qcow2,
serial-output, and QEMU binary hashes. INFO logs are compared byte-for-byte
after removing only each line's leading ISO-8601 wallclock timestamp. No
address, path, virtual time, scheduler count, or other number is normalized.

Both Python QEMU demos keep the QEMU-visible paths fixed at
`ignored/qemu-linux/qmp.sock`, `serial.sock`, and `hermit-snapshot.qcow2`.
Changing a socket or image path changes QEMU's initial stack and heap, so a
timestamped runtime directory would invalidate repeat comparisons before Linux
boots. A shared lock prevents concurrent demos from colliding on these paths.
The serial transcript is written to the fixed `serial.log` and copied into each
run's history directory after QEMU exits. Demo 5 also preserves the clean boot
image as `hermit-boot.qcow2` so Demo 6 can restore it into the fixed working
image before every command. Demo 5 recreates the fixed qcow2 path in place so
its host inode, which Detcore uses as a file-content scheduling identity, also
remains stable between runs.

The serial marker observer and QMP client run in `qemu_controller.py` as a
Hermit-managed parent of QEMU. Keeping that control process inside Hermit's
scheduler prevents host Python scheduling from choosing the `savevm` turn.
The outer demo process only streams the transcript and archives completed
artifacts; it does not inject input into the running VM.

QEMU stores a host-observation subsecond creation timestamp in each internal
snapshot entry. That field does not describe guest state: two snapshots with
identical VM state and VM clock can differ only in `date-nsec`. After QEMU
exits, the demos parse the version 3 qcow2 snapshot table, validate the named
entry, and zero only that field before hashing. The seconds-level creation
time, VM clock, and serialized VM state remain part of the exact comparison.

On first use, `demos/lib/qemu-assets.sh` downloads this content-addressed
kernel and verifies it before atomically populating the cache:

```text
manifold://test/tree/dev-hermit/qemu-kernels/e4b1c0248a31c7e1f7cb31d82a1a03d4e7cab408ee1b8e622dd897c17eae46a2/bzImage
sha256: e4b1c0248a31c7e1f7cb31d82a1a03d4e7cab408ee1b8e622dd897c17eae46a2
```

The helper also builds a small static BusyBox initramfs. Later runs reuse the
kernel only after checking its SHA again, so stale or host-specific cache
contents are replaced. `KERNEL_IMAGE` supplies a local copy of the same fixed
kernel for offline testing. `QEMU_KERNEL_MANIFOLD_PATH` and
`QEMU_KERNEL_SHA256` provide an explicit paired override for intentional kernel
updates; `BUSYBOX` and `QEMU_ASSETS` retain their existing overrides.

The boot and every resume use `--strict`, `--no-rcb-time`,
`--target-timeslice 100000`, and `--max-timeslice disabled`. Strict mode fails
closed on unsupported operations. This syscall-rich workload advances logical
time by deterministic scheduler check-ins and does not need PMU preemption,
whose hardware skid would otherwise perturb internal timing logs. The scripts
enable Detcore INFO logging so the raw log includes syscall entries and results
as well as scheduler records. Console output remains concise because it prints
only a timestamp-free tail.

### 6. QEMU Snapshot Resume

Demo 6 starts the same QEMU machine with `-loadvm hermit-boot`, connects to its
Unix serial socket, and injects one shell command. It prints the guest output
and timestamp-free Hermit INFO tail, then saves a post-command snapshot unless
`--no-save-snapshot` is passed. For example:

```bash
./demos/06-qemu-resume.py 'ls /'
./demos/06-qemu-resume.py 'cat /proc/cpuinfo'
./demos/06-qemu-resume.py 'uname -a'
./demos/06-qemu-resume.py --no-save-snapshot 'echo hello'
```

The command's SHA-256 selects its metadata directory. The first run anchors the
guest-output hash, post-command qcow2 hash, QEMU identity, and raw INFO log.
Repeating that command compares every field and reports the first log
divergence after stripping only the wallclock prefix.

## Scope And Next Steps

- Keep file contents and mount layouts fixed, prefer a minimal environment, and
  avoid external networking when asserting reproducibility.
- Use PMU timer preemption when exploring CPU-bound races. The portable chaos
  commands still find this syscall-rich demo failure without it.
- Treat version probes as launch coverage, not proof that every workflow of a
  program works.
- Benchmark the real workload; ptrace overhead varies with syscall frequency,
  thread count, scheduling, and logging.
- Demos 5 and 6 run inside the strict deterministic boundary and show their
  INFO-log evidence. Demo 6 performs the paired comparison for repeated guest
  commands without paying for a second Linux boot.

For full option and troubleshooting coverage, see the Hermit product
documentation under `hermit/docs/`. Hermit is BSD-licensed; see
`hermit/LICENSE`.
