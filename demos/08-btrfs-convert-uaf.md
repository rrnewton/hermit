# Demo 08: schedule-dependent btrfs-convert use-after-free

Demo 08 reproduces a real **userspace** concurrency bug — a heap
use-after-free in btrfs-progs' `btrfs-convert`. It reports a native baseline,
then requires a chaos abort, a clean fixed control and a repeated abort with
the same extracted ASAN signature on a recorded seed. No QEMU is involved; this is an ordinary
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

## Historical fixture results

From `demos/fixtures/demo08/` (btrfs-progs
v7.1, hermit primary checkout `103657d…`, `pop-tiny.img` ≈100 files).
These are the original hand-built fixture's measurements, not results for a
new build or a promise that its seeds work with another binary:

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

The script reports native buggy's actual status, runs chaos buggy on the crashing seed
(reproduces the UAF), chaos fixed on the same seed (clean — the differential),
then replays the crashing seed and compares the extracted ASAN signature. That
comparison covers the UAF error line, `task_period_wait`, `print_copied_inodes`
and `SUMMARY` lines; it does not compare the complete report or Hermit's logs.
Both complete outputs are retained. A nonzero native result remains visible;
the script's existing native baseline allows it to continue to the chaos controls.

It reports **SKIPPED (exit 0)** when the prebuilt assets are absent; the suite
records SKIP, which cannot satisfy a GREEN demo review. Set
`DEMO08_REQUIRE_ASSETS=1` to refuse missing assets. Provide them under `ignored/demo08-btrfs/` (or point
`DEMO08_DIR` elsewhere):

```
ignored/demo08-btrfs/buggy/btrfs-convert
ignored/demo08-btrfs/fixed/btrfs-convert
ignored/demo08-btrfs/pop-tiny.img
```

The default seed comes from the asset directory's `.crash-seed`. Preparation
writes the seed together with the buggy binary's SHA256 after completing the
buggy, replay and fixed controls; the demo refuses a recorded hash that differs
from the supplied binary. The value 15 is only the compatibility fallback for
original hand-built assets without that file. It is not the default for a
newly calibrated fixture.

Overrides: `DEMO08_DIR`, `DEMO08_ARTIFACTS`, `DEMO08_CRASH_SEED` (explicit seed),
`DEMO08_TIMEOUT` (default 90), `HERMIT_RELEASE`, `SAFEHERMIT`.

`DEMO08_TIMEOUT` is not the demo's budget alone: `scripts/prepare-demo08-assets.sh`
caps each calibration run at the same value, and refuses a
`DEMO08_CALIBRATION_TIMEOUT` above it, so a seed can never be calibrated above the
budget the demo will apply to it.

## Build recipe

Use `scripts/prepare-demo08-assets.sh` to build the fixtures and calibrate a
seed. It pins btrfs-progs **v7.1** to
`4ab0e80be9e3bb1db2e6038e6d4316d35fb7ba8b`, builds both variants, creates
the populated image and records a seed only after the required controls.
The build consumes these repository files:

- `demos/fixtures/demo08/buggy/common/task-utils.c` and
  `demos/fixtures/demo08/fixed/common/task-utils.c` — the two
  teardown variants (both carry the pipe harness; they differ only in
  detach/no-join vs. join).
- `demos/fixtures/demo08/{buggy,fixed}/common/task-utils.h` — the `struct periodic_info` fields
  the harness adds (`wait_write_fd`, `stop`).
- `demos/fixtures/demo08-convert-main-v7.1.patch` — the shared `convert/main.c`
  changes: bake in `__asan_default_options`, replace the irrelevant timer
  period argument and observe the `stop` flag in the progress loop.

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

The demo's invocation through the parent wrapper is:

```bash
timeout 90 "$SAFEHERMIT" "$HERMIT_RELEASE" --log=error run --chaos --sched-seed <S> --no-virtualize-cpuid -- <variant>/btrfs-convert <fresh-image-copy>
```

(`--no-virtualize-cpuid` because CPUID faulting is unavailable on the demo
hosts; each run converts a fresh reflink copy of the image.)
