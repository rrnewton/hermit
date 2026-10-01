# Demo 9: Boot BusyBox in QEMU, from a single script

This is the smallest whole-machine demo: one shell script that boots a pinned
Linux kernel with a BusyBox userland inside QEMU, with QEMU running under
`hermit run --strict`. The guest runs a short fixed workload (a kernel name
query, a directory listing, a shell pipeline, an arbitrary-precision
calculation of pi, and a SHA-256 of its own BusyBox binary) and powers off.
Unlike demos 5 to 7 it needs no Python and saves no snapshot, which makes it a
good first check that QEMU works under Hermit on your machine.

## Prerequisites

- `hermit` on your `PATH`, built from this checkout (see
  [Setup](../README.md#setup)).
- `qemu-system-x86_64`, a statically linked BusyBox (on `PATH` or given with
  `BUSYBOX=`), `curl` for the one-time kernel download (not needed with
  `KERNEL_IMAGE=`), and the usual command-line tools the scripts check for:
  `cpio`, `file`, `find`, `gzip`, `install`, `sha256sum`, `sort`, `stat`,
  `tee`, `timeout`, `touch`, `wc`.
- User-space access to the CPU's hardware performance counters. `--strict`
  uses them to switch between QEMU's threads at repeatable points.
- `ptrace` must be allowed (it is blocked by some container runtimes and by
  strict Yama settings).
- For the two-run check (`VERIFY=1`), also `jq`.
- Time: one boot takes about 160 seconds on the host in the timing notes below,
  and `VERIFY=1` about 320 seconds. A raised `SKID_MARGIN` makes the boot too
  slow to finish (see
  [Known limitation](#known-limitation-a-late-performance-counter-interrupt-has-no-tested-remedy)).

## Run it

From the repository root:

```bash
demos/09-qemu-busybox/run.sh
```

The same demo runs with `make -C demos demo9`. The first run downloads the
pinned kernel from the Hermit GitHub releases into `target/qemu-busybox/`,
checks its SHA-256, and builds the initramfs; later runs reuse the kernel.

To have Hermit boot the machine twice and compare the two runs:

```bash
VERIFY=1 DEMO_TIMEOUT_SECONDS=900 demos/09-qemu-busybox/run.sh
```

`VERIFY=1` adds `--verify --verify-strict --verify-json` to the Hermit command.
The script then reads Hermit's JSON verdict and requires that the runs matched,
that the comparison covered the full event logs without filtering, and that
both logs contain the same, nonzero number of messages. Two empty logs would
also compare equal, which is why the count is checked. This mode runs the boot
twice and then compares about 700,000 log messages. On the host in the timing
note below it took 320 seconds, against about 160 seconds for a single boot.
Nothing from the guest appears until both boots and the comparison have
finished. Hermit then prints the first run's console.

To run the launcher by hand once the kernel and initramfs exist:

```bash
hermit run --strict --epoch=2026-01-01T00:00:00Z -- demos/09-qemu-busybox/boot_qemu.sh target/qemu-busybox/bzImage target/qemu-busybox/initramfs-busybox.cpio.gz
```

## What you will see

Observed on 2026-09-30 with the cached kernel (a later run), QEMU 10.1.2, and
`/usr/sbin/busybox` from the `busybox-1.35.0-2.el9` package:

```text
kernel ready: .../target/qemu-busybox/bzImage (cached)
busybox=.../busybox
busybox_sha256=e35db14651077c08598fbc3259609b2db398e5b7dcf07b28f1f3156118bcc081
initramfs=.../target/qemu-busybox/initramfs-busybox.cpio.gz
initramfs_sha256=5515b4bced678c4d22ff54dafd1676f06b8e254f1656d2994018df24aa1e9698
entries=417
bytes=765044
backend=ptrace verify=off log=info relaxations=none
pmu_skid_margin=auto
hermit=.../hermit (hermit 0.2.0 (2026-09-30, gdc92644f96f4-dirty))
qemu=/usr/bin/qemu-system-x86_64
kernel=.../target/qemu-busybox/bzImage
initramfs=.../target/qemu-busybox/initramfs-busybox.cpio.gz
console=.../target/qemu-busybox/console.log
info=.../target/qemu-busybox/hermit-info.log
stderr=.../target/qemu-busybox/hermit-stderr.log
kernel_sha256=e4b1c0248a31c7e1f7cb31d82a1a03d4e7cab408ee1b8e622dd897c17eae46a2
initramfs_sha256=5515b4bced678c4d22ff54dafd1676f06b8e254f1656d2994018df24aa1e9698
[    0.000000] Linux version 6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79 (...) #1 SMP Thu Dec 18 12:27:13 PST 2025
[    0.000000] Command line: console=ttyS0 panic=-1 rdinit=/init
...
[    1.804517] Run /init as init process
HERMIT-QEMU-BUSYBOX-START
Linux (none) 6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79 #1 SMP Thu Dec 18 12:27:13 PST 2025 x86_64 x86_64 x86_64 GNU/Linux
--- root filesystem ---
bin
dev
etc
home
init
linuxrc
proc
root
sbin
sys
tmp
usr
--- four-stage pipeline ---
3eca7ea48b0da0ad30bee679c92c7b68d487547068b6914d10a64e8cedb03f51  -
--- pi (bc -l, scale=10) ---
3.1415926532
--- busybox sha256 ---
e35db14651077c08598fbc3259609b2db398e5b7dcf07b28f1f3156118bcc081  /bin/busybox
HERMIT-QEMU-BUSYBOX-PASS
[    1.906387] ACPI: PM: Preparing to enter system sleep state S5
[    1.906409] reboot: Power down
console_sha256=4b4aba392c399904f03eeb3700cd08a02151bc757f788e5963c03ed78b4c60db
PASS: BusyBox userspace completed under Hermit/QEMU (ptrace backend, --strict)

=== Demo 9: QEMU BusyBox Boot: SUCCESS ===
```

Elided: the paths of your checkout, Hermit, and BusyBox; the kernel builder's
host name in the `Linux version` line; and the 352 kernel boot messages
between `Command line:` and `Run /init`. BusyBox `ls` colors the directory
listing on the serial console; the color escape codes are left out above. The
first run prints the kernel download instead of the `(cached)` line.

The BusyBox, initramfs, and console hashes depend on your BusyBox binary, and
the console hash can also change with your QEMU and Hermit. The kernel hash, the
pipeline hash, and pi come from pinned inputs and should match exactly. The
console hash was the same, `4b4aba39...`, on a `run.sh` run, in both boots of
the `VERIFY=1` run below, on `make -C demos group2`, and on a `run.sh` run
started as `yes | DEMO_EXTRA_PROBE=1 demos/09-qemu-busybox/run.sh`, that is,
with a pipe on standard input and one extra variable in the environment. That last run's
event log in `hermit-info.log` was also identical to the first run's once the
wall-clock time at the start of each line was removed.

With `VERIFY=1`, the header says `verify=strict` and adds a
`verify_json=.../target/qemu-busybox/verify.json` line. Hermit prints its
comparison summary on standard error, saved in `hermit-stderr.log`, before the
console:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 699686 | 699686 messages total
Logs contain 699685 | 699685 detcore-specific messages
Logs contain 699685 | 699685 INFO messages
Logs contain 696293 | 696293 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (699685 | 699685 INFO messages compared).
Logs contain 1 | 1 scheduler empty-run-queue kick messages
Logs contain 0 | 0 scheduler COMMIT records reading /proc/self/maps
:: comparison=BitwiseInfoV1 relaxations=none
:: Success: deterministic. Determinism verified.
```

The script's own lines end with:

```text
console_sha256=324712862e60da9b4fb27b977f550e824a3679ec240a927a752eb02721adb0e5
compared_info_messages=714635
PASS: two runs of the BusyBox boot produced identical output and event logs (ptrace backend)

=== Demo 9: QEMU BusyBox Boot: SUCCESS ===
```

Standard error and standard output reach the terminal separately, so Hermit's
last two summary lines can land inside the console output. In one observed
run `:: comparison=BitwiseInfoV1 relaxations=none` came out in the middle of
the kernel's `Calibrating delay loop` message, and
`:: Success: deterministic. Determinism verified.` on the next line; where they
land varies from run to run. One `VERIFY=1` run on 2026-10-01 with Hermit build
`gf35595861a09` (hermit main `1139c661ede3` plus demo commits that change no
Hermit source) printed the `console_sha256` and `compared_info_messages` lines
shown above. Its `verify.json` reported `"verdict": "matched"` and
`"bitwise_parity": true`, and its two boots, `runtime.run1` and
`runtime.run2`, gave identical values: `scheduler_turns` 40,383, `syscalls`
257,606, and `virtual_nanoseconds` 194,572,515,695, that is, 194.572515695
seconds of virtual time. Earlier builds gave other values: two `VERIFY=1` boots
with build `gca6afb1b3124` gave 37,385 turns, 251,622 system calls, and
193.051453375 seconds. This is an L2 result for the ptrace backend at log level
`info` with no relaxations.

The by-hand launcher command above prints the same kernel messages and workload
to the terminal, preceded by Hermit's
`hermit: virtual-time epoch=2026-01-01T00:00:00+00:00 source=explicit; ...`
line and one warning:

```text
... WARN detcore: [dtid 3] cpuid leaf 0x8000000 subleaf 0x90b82201 not in deterministic table; returning zero result
```

(the timestamp is elided). Its workload section, from
`HERMIT-QEMU-BUSYBOX-START` to `HERMIT-QEMU-BUSYBOX-PASS`, was byte-identical
to `run.sh`'s. The whole transcript was not: some kernel timestamps differed in
their last digit, as did the order of the six `SATA link down` lines. That
command runs QEMU without `run.sh`'s explicit QEMU path argument, without
`--log info`, without `--base-env=minimal`, and with whatever standard input
your shell gives it, so do not compare its hash with `run.sh`'s. Observed on
2026-09-30 with the current scripts; it took 154 seconds.

On 2026-07-27, with QEMU 10.1.0, the same BusyBox, and the same initramfs
(SHA-256 `5515b4bced678c4d22ff54dafd1676f06b8e254f1656d2994018df24aa1e9698`),
an earlier version of this script also printed pi as `3.1415926532`. Hermit
scheduled six shell and QEMU
threads for 38,088 turns over 181.740147850 seconds of virtual time, and the
console transcript hashed to
`f9a42014fac177223f08d5e722a8c6d88ae3b79eb0f1fab95bbdcb15487fbab3`.

## What to notice

- The kernel's own clock checks pass: the script fails the run if the guest
  kernel reports a skewed or unusable clock source (for example
  `Marking TSC unstable`). The guest's time comes from QEMU's instruction count,
  and QEMU's time comes from Hermit.
- The workload marker `HERMIT-QEMU-BUSYBOX-PASS` must appear. Determinism alone
  is not enough, since two identical failed boots would also match.
- The console transcript is saved as `target/qemu-busybox/console.log` and its
  SHA-256 is printed. With the same kernel, BusyBox, QEMU, and Hermit, another
  run prints the same hash. That holds because `run.sh` fixes three inputs that
  Hermit otherwise takes from the host, each of which changed the hash or the
  event log on the host in the timing notes:
  - The clock. `run.sh` passes `--epoch=2026-01-01T00:00:00Z`. Without an
    epoch, Hermit starts its virtual clock at the host's current time and
    QEMU's real-time clock hands that time to the guest. Two runs made without
    the flag printed different hashes: the kernel's `audit(...)` timestamp and
    its `setting system clock to` line differed, some later kernel timestamps
    differed in their last digit, and the six `SATA link down` lines came out
    in a different order. The workload output was the same.
  - Standard input. QEMU's `-serial stdio` reads it, so `run.sh` gives Hermit
    `/dev/null`. An earlier version of the script inherited standard input, and
    one run whose standard input was open for both reading and writing on a
    pipe or socket printed hash `c25aa724...` instead of the hash every other
    run printed. Several kernel timestamps in it were one microsecond apart
    from the other runs'. Its event log first differed where QEMU asked for the
    current position of standard input, which fails on a pipe (`ESPIPE`).
  - The environment. `run.sh` passes `--base-env=minimal`. With Hermit's default
    `--base-env=host` the guest gets your whole environment, and the number of
    variables changes how many branches the dynamic loader executes before the
    program starts. Hermit uses that branch count to place its preemption
    points. In a test with `/bin/true` under `--base-env=host`, the program's
    third clock read came at 36,095 retired branches, and at 36,265 with one
    more variable set; under `--base-env=minimal` it came at 3,187 in both
    cases.

  A fourth input showed up only in a `VERIFY=1` comparison. An earlier
  `boot_qemu.sh` always looked up its own directory with bash's `cd`, which
  examines every directory above the checkout, including your home directory.
  During one `VERIFY=1` run another program on the host changed the home
  directory, whose reported size went from 11094 bytes in the first boot to
  11108 bytes in the second. The comparison failed on the two `newfstatat`
  records for the home directory out of 709,199 INFO messages.
  `boot_qemu.sh` now skips the lookup when `run.sh` passes the paths.
- Hermit's full event log goes to `target/qemu-busybox/hermit-info.log` rather
  than to your terminal, so the serial console stays readable. For one boot it
  was about 159 MB (774,469 lines); each run overwrites it. With `VERIFY=1`
  that file holds only Hermit's `virtual-time epoch=` line, because Hermit
  keeps the two runs' logs for its own comparison.

## How it works

`build-initramfs.sh` puts the BusyBox binary, its applet links, and the
[`init`](init) script into a compressed cpio archive, fixing everything that
normally varies between hosts: file order, owners, timestamps, archive inode
numbers, and the gzip header. The same BusyBox therefore always gives the same
initramfs.

[`boot_qemu.sh`](boot_qemu.sh) checks its inputs and then replaces itself with
QEMU, configured to leave nothing to chance: the `q35` machine with the `max`
CPU model, one virtual CPU on QEMU's software emulator, `-icount shift=0` so the
guest's clocks follow the instruction count, a real-time clock driven by that
virtual clock, no default devices, no network, and the serial console on
standard output. [`run.sh`](run.sh) runs that launcher under
`hermit --log info run --strict --epoch=2026-01-01T00:00:00Z --base-env=minimal`
with standard input from `/dev/null`, with no settings that relax determinism,
tees the console to `console.log`, and checks the result. It passes the
launcher the kernel, initramfs, and QEMU paths as arguments. Given the
initramfs path, `boot_qemu.sh` does not look up its own directory, because
bash's `cd` examines every directory on the way there and Hermit reports those
directories' sizes as they are. A directory's size changes whenever a file is
created or removed in it, for example in your home directory.

QEMU has several threads. Under `--strict` Hermit runs them one at a time,
preempts them at points counted in retired branch instructions, and answers
every clock and random-number request from its virtual sources.

### Known limitation: a late performance-counter interrupt has no tested remedy

Hermit places its preemption points by counting retired branch instructions
with the CPU's performance counters. The counter's interrupt can arrive a few
branches late (the counter's "skid"); Hermit reports that on standard error
with a line starting `HERMIT_SKID_OVERSHOOT`. Hermit stops each thread a margin
of branches early and single-steps the rest of the way, and `SKID_MARGIN`
passes a different margin to `hermit run --skid-margin`. A larger margin does
not relax determinism, but in this demo it makes the boot too slow to finish,
so this README gives no margin to use. On the AMD EPYC 9D85 host in the timing
notes, on 2026-09-30:

- Hermit's default margin for this processor is 1,000 branches. With it, no run
  reported a late interrupt, and the boot took about 160 seconds.
- The repository's skid-measurement tool (`tests/util/pmu_skid.c`) recommends
  twice the largest skid it observes. That recommendation depends on a few rare
  outliers: over 13 runs of the tool the 99th percentile stayed between 410 and
  900 branches, while the recommendation ranged from 1,404 to 273,012.
- With `SKID_MARGIN=140138`, one such recommendation, the default 300-second
  timeout expired before the guest kernel printed anything.
- With `SKID_MARGIN=17288` and a longer timeout, the run had completed 552
  scheduler turns after 15 minutes (with a second Hermit run active on the same
  host) and was stopped. A full boot takes about 37,000 to 40,000 turns:
  36,893 with an earlier version of the scripts, 37,385 with Hermit build
  `gca6afb1b3124`, and 40,383 with build `gf35595861a09`.

So on a host where the default margin produces `HERMIT_SKID_OVERSHOOT`, this
demo is not known to work. Demo 9 is therefore only partially verified: the
default run and the `VERIFY=1` comparison above passed, and no run with a
raised margin has completed.

What this demo does not cover: the guest has no disk, network, snapshot, or
record and replay. Demos [5](../05-qemu-boot/README.md),
[6](../06-qemu-resume/README.md), and [7](../07-drgn-kernel/README.md) add a
command disk, snapshots, and kernel inspection.

Controls (environment variables):

| Variable | Default | Meaning |
| --- | --- | --- |
| `KERNEL_IMAGE` | (download) | Use a local `bzImage` instead of the pinned download. |
| `QEMU_KERNEL_URL`, `QEMU_KERNEL_SHA256` | the pinned release | Pin a different kernel; set both together. |
| `INITRAMFS_IMAGE` | (built) | Use an existing initramfs instead of building one. |
| `BUSYBOX` | `busybox` on `PATH` | A statically linked BusyBox for the initramfs. |
| `QEMU_BIN` | `qemu-system-x86_64` on `PATH` | The QEMU binary. |
| `DEMO_TIMEOUT_SECONDS` | `300` | Seconds before the run is stopped. |
| `VERIFY` | `0` | Set to `1` to run twice and compare. |
| `SKID_MARGIN` | (Hermit's default) | Passed to `hermit run --skid-margin`. No raised value has completed a boot; see the known limitation above. |
| `OUTPUT_DIR` | `target/qemu-busybox` | Where the kernel, initramfs, console, and logs are kept. |
