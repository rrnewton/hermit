# Demo 08: schedule-dependent btrfs-convert use-after-free

Demo 08 reproduces a real **userspace** concurrency bug — a heap
use-after-free in btrfs-progs' `btrfs-convert` — that native (blind) execution
almost never hits, but hermit's chaos scheduler lands deterministically on a
recorded seed and replays bit-for-bit. No QEMU is involved; this is an ordinary
multithreaded userspace program under `hermit run --chaos`.

## The bug

`btrfs-convert` spawns a background *progress* subthread
(`convert/main.c:print_copied_inodes`) that repeatedly dereferences a shared
`struct task_info *info` while the main thread copies inodes. Before upstream
commit **73e211a7**, the teardown path was:

- `task_start()` called `pthread_detach()` on the subthread, and
- `task_stop()` did **not** `pthread_join()` it,

so `task_deinit()` could `free(info)` while the detached subthread was still
reading `*info`. Whether the crash happens depends entirely on the thread
interleaving at teardown — a textbook schedule-dependent use-after-free. The
**fix** (this demo's differential) is exactly 73e211a7: do not detach, and
`pthread_join()` the subthread before `task_deinit()` frees `info`.

Two prebuilt binaries drive the demo:

- `buggy/btrfs-convert` — the pre-73e211a7 detach + no-join teardown.
- `fixed/btrfs-convert` — the 73e211a7 no-detach + join teardown.

Both are compiled with AddressSanitizer so the latent UAF becomes an observable
`abort()` (exit 134) with a precise report, rather than a silent, occasionally
corrupt read.

## The observability adaptation (stated plainly)

Two host-vs-hermit realities would otherwise keep this historical bug dormant
under hermit, so the demo binaries carry a small, clearly commented harness.
It is applied **identically to the buggy and fixed variants**, so the only
behavioral difference between them remains the real bug (detach/no-join vs.
join). The full source is vendored under
`demos/fixtures/demo08/`.

1. **The progress timer never fires under hermit.** The historical code paces
   the subthread with a wall-clock `CLOCK_MONOTONIC` `timerfd`. Hermit
   virtualizes `CLOCK_MONOTONIC` to logical (RCB) time, which barely advances
   during this I/O-bound conversion, so the timer never ticks: the subthread
   parks forever at its `read()` and the UAF window never opens. The harness
   replaces the timerfd with a **pipe** — the subthread blocks cheaply on the
   read end during `copy_inodes()`, and `task_stop()` writes a single "final
   tick" byte to wake it for the one last loop iteration that races `free()`.
   A single bounded wake (not a close-driven EOF, which would make the thread
   perpetually runnable and trigger a hermit context-switch storm) is what lets
   the scheduler drive the teardown interleaving cleanly.

2. **ASAN options must be baked in.** `ASAN_OPTIONS` from the host environment
   does not reach the guest under hermit, so the binaries define
   `__asan_default_options()` (`abort_on_error=1`, `disable_coredump=0` to avoid
   the `setrlimit(RLIMIT_CORE)` hermit rejects, leak detection off).

Neither change alters which teardown ordering is safe; they only make the
existing race *reachable and observable* under hermit's logical clock. The
racing access itself — the subthread's load/store of `*info` after the main
thread frees it — is the genuine, unmodified 73e211a7 bug.

## Results

**Which seeds crash is a property of the inputs, not of this document.** Two
sweeps of the same 0–31 range, against a byte-identical `buggy/btrfs-convert`,
disagree about which seeds reproduce. Read the numbers below as two dated
measurements, and derive your own seed with
`scripts/prepare-demo08-assets.sh` rather than copying one from here.

First sweep — btrfs-progs v7.1, hermit primary checkout `103657d…`,
`pop-tiny.img` ≈100 files:

| Execution | Runs | UAF crashes | Notes |
|---|---|---|---|
| native buggy | 40 | **0** | bug dormant under blind execution |
| native fixed | 20 | 0 | — |
| chaos buggy, seeds 0–31 | 32 | **2** (seeds **15**, **19**) | schedule-dependent |
| chaos fixed, seeds 0–31 | 32 | **0** | fix closes the window |

Seeds 5 and 30 are pathologically slow chaos *schedules* for this workload and
hit the per-run timeout in **both** variants; they are a hermit-chaos artifact,
not the bug. Buggy seed 15 replayed 3× produced a **byte-identical** guest ASAN
report (same faulting heap address, PC, stack, and shadow bytes); only hermit's
own host-side log lines vary.

Second sweep — 2026-08-31, the **same** fixture
(`buggy/btrfs-convert` sha256 `6e660b87…`), hermit release built at
`00ed139b` (sha256 `7d559e36…`), devbig014, AMD EPYC 9D85:

| Execution | Runs | UAF crashes | Notes |
|---|---|---|---|
| chaos buggy, seeds 0–31 | 32 | **4** (seeds **3**, **6**, **10**, **13**) | 15 and 19 are clean here |
| chaos fixed, seeds 3/6/10/13 | 4 | **0** | progress-thread path engaged on all four |

Seeds 16 and 25 hit the 150 s per-run timeout without reaching the progress
thread. Seeds 3 and 13 report the UAF on a thread whose **process still exits
0**, and ASAN does not finish writing frames before the process goes — which is
why both this demo and the calibrator detect on the report text rather than the
exit status, and why the calibrator prefers a seed whose report reached its
`SUMMARY` line.

**The seed set is per-input down to the image path.** Seed 3 reproduces the
truncated report at the image path that sweep used, twice, with a byte-identical
error line — and does *not* reproduce at a different image path. That is not
flakiness: hermit's determinism is per-input, and the path length shifts the
initial heap layout, which is the same reason Step 4 replays over the *same*
image path rather than a fresh one. Treat a crashing seed as belonging to the
whole input set it was derived from.

What the pair of sweeps establishes is narrow and worth stating exactly: **the
fixture alone does not determine which seeds crash.** It does *not* establish
which other input does. No older hermit was built to isolate the variable, and
host, kernel and toolchain differed too. That is why the recorded seed is
verified by replaying it rather than by comparing hashes — see
`scripts/prepare-demo08-assets.sh`.

The crash is the textbook 73e211a7 UAF:

```
ERROR: AddressSanitizer: heap-use-after-free  READ of size 8  thread T1
  #0 task_period_wait      common/task-utils.c:154
  #1 print_copied_inodes   convert/main.c:170
freed by thread T0 here:  free -> task_deinit -> do_convert
```

## Run

```bash
./demos/08-btrfs-convert-uaf.sh
```

The script runs native buggy (clean), chaos buggy on the crashing seed
(reproduces the UAF), chaos fixed on the same seed (clean — the differential),
then replays the crashing seed and confirms the guest ASAN report is identical.

The seed comes from `<asset-dir>/.crash-seed`, written by
`scripts/prepare-demo08-assets.sh`. **There is no built-in default seed**: as the
two sweeps above show, a hardcoded one is wrong on most hosts, and the previous
default of 15 does not reproduce at `00ed139b`. When the recorded seed no longer
reproduces, the demo says `STALE SEED RECORD`, names the recorded and present
identities, re-derives a seed through the calibrator, and retries. It reports a
regression only when a seed **derived from the inputs actually present** fails to
reproduce.

It **skips cleanly (exit 0)** when the prebuilt assets are absent, so it is safe
in `make all` — though `demos/run-all.sh` records that as `SKIP`, not `PASS`,
because a gated-out demo demonstrated nothing. Provide the assets under
`ignored/demo08-btrfs/` (or point `DEMO08_DIR` elsewhere):

```
ignored/demo08-btrfs/buggy/btrfs-convert
ignored/demo08-btrfs/fixed/btrfs-convert
ignored/demo08-btrfs/pop-tiny.img
```

Overrides: `DEMO08_DIR`, `DEMO08_ARTIFACTS`, `DEMO08_TIMEOUT` (defaults to the calibrator budget, 150),
`HERMIT_RELEASE`, and `DEMO08_CRASH_SEED` to drive one seed by hand (which also
turns off re-derivation, so a miss is reported rather than repaired).

## Build recipe

Build btrfs-progs **v7.1** twice, applying the vendored sources from
`demos/fixtures/demo08/`:

- `src/buggy/common/task-utils.c` and `src/fixed/common/task-utils.c` — the two
  teardown variants (both carry the pipe harness; they differ only in
  detach/no-join vs. join).
- `src/{buggy,fixed}/common/task-utils.h` — the `struct periodic_info` fields
  the harness adds (`wait_write_fd`, `stop`).
- `src/convert-main.c.changes.md` — the two `convert/main.c` edits common to
  both variants (bake in `__asan_default_options`; observe the `stop` flag in
  the progress loop).

Compile each variant with ASAN:

```bash
make EXTRA_CFLAGS='-fsanitize=address -fno-omit-frame-pointer -g -O1 -D_FORTIFY_SOURCE=0' \
     EXTRA_LDFLAGS='-fsanitize=address' btrfs-convert
```

Build a small populated ext4 image (block size must be ≥ 4096 or btrfs-convert
rejects it):

```bash
mkfs.ext4 -F -q -b 4096 -N 200 -d <some-populated-dir> pop-tiny.img   # 256 MiB
```

The validated hermit invocation the demo uses is:

```bash
hermit run --chaos --sched-seed <S> --no-virtualize-cpuid --strict -- <variant>/btrfs-convert <fresh-image-copy>
```

Backend `ptrace`; log level `error`; the single relaxation is
`--no-virtualize-cpuid`, because CPUID faulting is unavailable on the demo
hosts. Each run converts a fresh reflink copy of the image, and
`scripts/prepare-demo08-assets.sh` uses this identical flag set — a seed derived
under one flag set is not evidence about another.

**`--strict` is load-bearing, and running without it was a real defect.**
`btrfs-convert` generates a random UUID for the target filesystem. Without
`--strict`, hermit does not virtualize that: measured 2026-08-31, three
non-strict runs of seed 6 produced two different target UUIDs, and within a
single demo run the seed reproduced the UAF at Step 2 and *missed* at Step 4,
the two transcripts differing at the UUID line. Four `--strict` runs produced
target UUID `10708a9d-7517-44b2-8a5b-dc05ab4ae2fd` every time and reproduced the
UAF 4/4, at roughly 9 s each against roughly 7 s without. A demo whose headline
claim is bit-for-bit replay must not run in the mode that does not guarantee it.

Step 4 therefore compares the **entire guest transcript**, with only
safehermit's host-side accounting lines removed, rather than the four-line
filtered ASAN extract it used to compare. That extract does not contain the UUID
line, so it would have reported "byte-identical" across two runs that genuinely
differed.
