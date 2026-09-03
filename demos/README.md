# Hermit Demo Walkthrough

This walkthrough demonstrates reproducible Linux execution from the Hermit
repository. Hermit runs unmodified x86-64 Linux programs under the
[Reverie](https://github.com/facebookexperimental/reverie) ptrace backend and
controls common sources of nondeterminism, including thread scheduling, time,
random data, CPUID results, address layout, and selected file metadata.

The demo materials and Hermit source live in this repository. The walkthrough
covers eight confirmed workflows:

1. repeat an execution with stable guest-visible inputs;
2. record an execution and replay it, with or without GDB;
3. search seeded thread schedules for a concurrency failure;
4. bisect two schedules to identify the events that change the outcome;
5. boot Linux in QEMU and save a live snapshot under Hermit's strict profile;
6. resume that snapshot and inject repeatable commands over its serial port;
7. inspect and advance the restored kernel without advancing it during reads;
8. expose a schedule-dependent use-after-free in btrfs-convert that blind
   execution misses, and show the fix closing it on the same seed.

> [!WARNING]
>
> Hermit is not a security boundary, and it does not make changing files or
> external network responses deterministic. Record/replay support is
> experimental and narrower than `hermit run` compatibility.

## Requirements

Use an x86-64 Linux host with Rust nightly (selected by the repository's
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
Cargo starts and points back to `make install-deps`. Running `make` verifies the
pinned submodules and builds the debug Hermit binary with every backend.

The demos use private temporary and ignored build-artifact directories. Demos
5-7 additionally need Python 3, `qemu-system-x86_64`, `qemu-img`, a statically
linked BusyBox, `cpio`, `gzip`, `curl`, `file`, and `sha256sum`. Install the
QEMU-demo packages with one of:

```bash
# Debian/Ubuntu
sudo apt install python3 qemu-system-x86 qemu-utils busybox-static \
  cpio gzip curl file

# Fedora
sudo dnf install python3 qemu-system-x86-core qemu-img busybox \
  cpio gzip curl file
```

On CentOS/RHEL, install `qemu-kvm-core`, `qemu-img`, and the EPEL `busybox`
package. That BusyBox is static but is commonly installed at
`/usr/sbin/busybox`; set `QEMU_BIN=/usr/libexec/qemu-kvm` and
`BUSYBOX=/usr/sbin/busybox` when those binaries are not on `PATH`. Run
`./demos/lib/qemu-assets.sh --check` for one complete, zero-build dependency
report before starting Demo 5.

Demo 5 downloads a fixed public kernel, verifies its SHA-256, and caches it with
the generated initramfs and qcow2 snapshot disk under `ignored/qemu-linux`. From a
checkout under `/tmp`, the default moves to the checkout-scoped
`/var/tmp/hermit-qemu-strict-l2-$UID-<checkout-hash>` so concurrent clones do not
share or clean each other's artifacts and Hermit's private guest `/tmp` does not
hide those QEMU inputs. The Hermit
command also identity-mounts host `/tmp`, keeping the checkout-local controller
and per-run paths visible.

When a deep checkout would put a QMP socket over Linux's AF_UNIX pathname
limit, Demos 5 and 6 move only that socket under `/var/tmp`, which remains
visible to QEMU under Hermit. Set `QEMU_SOCKET_DIR` to another short writable
host-visible directory outside `/tmp` when `/var/tmp` is unavailable.

## Layout

```text
Makefile                    # dependency install/check and Hermit build
demos/
  README.md                 # this walkthrough
  QEMU_BUSYBOX.md           # standalone BusyBox/QEMU example
  qemu-busybox.sh           # standalone BusyBox/QEMU runner
  common.sh                 # checks dependencies, builds Hermit, defines helpers
  01-deterministic-run.sh   # stable inputs, --verify
  02-record-replay.sh       # record, list, replay, replay under GDB
  03-chaos-concurrency.sh   # seeded schedules, save/replay a failing schedule
  04-schedule-bisection.sh  # portable syscall-boundary hermit analyze
  05-qemu-boot.py           # boot, snapshot, metadata, repeat verification
  06-qemu-resume.py         # resume, command snapshot, repeat verification
  07-drgn-kernel.sh         # reproducible kernel task-list evolution
  08-btrfs-convert-uaf.sh   # schedule-dependent btrfs-convert UAF, and its fix
  WALKTHROUGH.md            # commands and expected output for the demo suite
  lib/
    demo_common.py          # hashes, metadata, QMP, serial, strict log diff
    qemu_controller.py      # deterministic in-Hermit QEMU serial/QMP controller
    qemu-assets.sh          # portable kernel/initramfs helper + preflight
    qemu-snapshot.sh        # QMP, snapshot, and stable-log helpers
```

Demos 1-4 source `demos/common.sh`, which locates this repository, builds the
release and debug binaries, and defines the shared `run_hermit`,
`verify_hermit`, and `chaos_run` wrappers. The Python QEMU demos use
`demos/lib/demo_common.py` for QMP, serial streaming, hashes, metadata, and
repeat comparison. `run_hermit` disables CPUID virtualization, so CPUID is a
host input in those commands. It leaves PMU timer preemption enabled by default;
`HERMIT_DEMO_MAX_TIMESLICE=disabled` selects syscall-boundary-only scheduling
for hosts without accessible performance counters. `chaos_run` uses
syscall-boundary-only scheduling. `verify_hermit` keeps PMU-based preemption on
(the racy verify guest is only reliably determinized with real preemption) and
raises the log level to `info` (at `--log=error` the execution log that
`--verify` compares is empty). The demo-1 `--verify` step requires the PMU;
demo 4 only requires it when `ANALYZE_PREEMPTION_TIMEOUT` enables preemption.
Demo 2 uses the debug Hermit binary from the same recorded source revision. At
Hermit `e85aaf9654983116ac26ae02beb8f95f7c46f02f`, the release binary returns
`EFAULT` while replaying the bootstrap exec; the debug binary and its
record/replay integration test complete successfully.

## Quick Start

Clone current `main` with its pinned submodules, install dependencies once per
host, and build Hermit:

```bash
git clone --recurse-submodules https://github.com/rrnewton/hermit.git
cd hermit
make install-deps
make
```

Run the demo suite locally with one command. It prepares the required assets,
then runs demos 1-8 with a PASS/FAIL row and log for each demo:

```bash
scripts/prepare-demo08-assets.sh
make demos
```

Demo logs and the machine-readable summary are written under
`target/demo-sweep/`. The manually dispatched Demo Hot Path workflow runs the
suite at an exact Hermit commit, preserves per-demo logs, and stays failed on
every demo red.

Demo 8 seed calibration writes `ignored/demo08-run/calibration.tsv` plus one
retained output per attempted seed. Each row records the seed, whether the
`print_copied_inodes` progress-thread path engaged, whether ASAN reported the
planted heap use-after-free, the exit status, whether that evidence qualifies,
and the output path. A seed qualifies only when the path engaged and a complete
ASAN report ended in the expected abort status 134; rc=0 and timeout-truncated
rc=124 reports are refused. The summary states `engagement=N/M`,
`uaf_hits=H/M`, and `qualified=Q/M`. A cached crash seed is replayed and must
supply qualifying 1/1 evidence; it is never trusted merely because the cache
file exists. Zero engagement is a refused `NO-RESULT`, not a clean sweep.

Run each demo individually so its output and result remain easy to inspect:

```bash
./demos/01-deterministic-run.sh
./demos/02-record-replay.sh
./demos/03-chaos-concurrency.sh
./demos/04-schedule-bisection.sh
./demos/05-qemu-boot.py
./demos/06-qemu-resume.py 'ls /'
./demos/07-drgn-kernel.sh
./demos/08-btrfs-convert-uaf.sh
```

Demo 8 needs prebuilt ASAN `btrfs-convert` fixtures; build them first with
`scripts/prepare-demo08-assets.sh`. Without them it reports SKIPPED and exits 0,
or fails when `DEMO08_REQUIRE_ASSETS=1` is set, as the hosted gate sets it.

Demo 4 is intentionally slow. Demo 5 must complete before Demos 6 and 7 because
it creates their baseline QEMU snapshot.

Set `DEMO_SKIP_BUILD=1` to reuse an existing `target` build, or export
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
SHA-256, and timestamp. The first run becomes `run-metadata.json` under the
resolved QEMU asset directory; later runs compare their exact qcow2,
serial-output, and QEMU binary hashes. INFO logs are compared byte-for-byte
after removing only each line's leading ISO-8601 wallclock timestamp. No
address, path, virtual time, scheduler count, or other number is normalized.
`demo_common.py` owns the complete kind-specific `run-metadata.json` type.
Schema 2 requires every value used by repeat comparison, so two omitted values
cannot compare equal and report a pass. Retained schema-1 boot and resume rows
remain readable; the three older resume rows made without a saved snapshot may
also lack the QEMU binary digest that later schema-1 writers added.

The demo-6 resume path keeps the QEMU-visible paths fixed under the resolved
QEMU asset directory: `qmp.sock`, the `serial-pipe.in`/`serial-pipe.out` FIFO
pair, and `hermit-snapshot.qcow2`. (Resume uses a `-serial pipe:` FIFO pair, not a
unix socket, because a socket chardev's poll fd starves the -icount vCPU under
`hermit --no-rcb-time`; boot uses a `-serial file:` transcript for the same
reason.) Changing a socket or image path changes QEMU's initial stack and heap, so a
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
https://github.com/rrnewton/hermit/releases/download/qemu-kernel-e4b1c0248a31c7e1f7cb31d82a1a03d4e7cab408ee1b8e622dd897c17eae46a2/bzImage
sha256: e4b1c0248a31c7e1f7cb31d82a1a03d4e7cab408ee1b8e622dd897c17eae46a2
```

The helper also builds a small static BusyBox initramfs. Later runs reuse the
kernel only after checking its SHA again, so stale or host-specific cache
contents are replaced. `KERNEL_IMAGE` supplies a local copy of the same fixed
kernel for offline testing. `QEMU_KERNEL_URL` and `QEMU_KERNEL_SHA256` provide
an explicit paired override for intentional public-kernel updates;
`QEMU_KERNEL_MANIFOLD_PATH` selects an optional internal mirror.
`QEMU_KERNEL_MANIFOLD_PATH` takes precedence over the default public URL.
`BUSYBOX`, `QEMU_BIN`, and `QEMU_ASSETS` retain their existing overrides.

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
serial pipe, and injects one shell command. It prints the guest output
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
- Demos 5-7 run inside the strict deterministic boundary. Demos 5 and 6 show
  their INFO-log evidence, and Demo 7 proves that its drgn reads do not advance
  the guest. Demo 6 performs the paired comparison for repeated guest commands
  without paying for a second Linux boot.

The pre-existing standalone BusyBox/QEMU example remains available as
`demos/qemu-busybox.sh`; see `demos/QEMU_BUSYBOX.md` for its separate commands
and evidence. For full option and troubleshooting coverage, see `docs/`.
Hermit is BSD-licensed; see `LICENSE`.
