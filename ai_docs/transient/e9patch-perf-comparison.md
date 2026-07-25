# e9patch performance comparison

Measured: 2026-07-25

## Result

On ten bounded compute workloads, warm-cache e9patch strict execution was
effectively the same speed as ptrace strict execution:

- Unweighted geometric mean, e9patch / ptrace: **1.0023x (+0.23%)**.
- Sum of workload medians: **99.469s e9patch vs 99.710s ptrace (-0.24%)**.
- Rewritten-program geometric mean (`gcc`, `g++`, `rustc`, BusyBox):
  **1.0061x (+0.61%)**.
- Zero-site-program geometric mean: **0.9998x (-0.02%)**.

The data does not establish a meaningful steady-state performance difference
between the backends. That is consistent with the current architecture:
e9patch is a cached main-ELF preprocessing step followed by the same ptrace
Detcore runtime. First-use preprocessing is excluded from these numbers.

Both strict modes were expensive relative to native execution for these short,
branch-heavy fixtures: the geometric means were **38.59x native for ptrace**
and **38.68x native for e9patch**. These ratios include Hermit startup and are
specific to the bounded workloads below; they are not general application
slowdown factors.

## Snapshot

- Measured commit: `6cadacb0e6796a2a97ac7b0f215ccf65bba3ea15`
  (`Preserve partial deterministic reads (#689)`).
- `origin/main` advanced after measurement to `f0b9eff2`; that commit changes
  only `validate.sh`, so product code and the measured binaries did not change.
- Host: x86_64 Linux `6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79`.
- CPU: AMD EPYC 9D85 158-Core Processor; all samples pinned to CPU 0.
- PMU policy: `/proc/sys/kernel/perf_event_paranoid=1`.
- Hermit: release build, SHA-256
  `64092d9dc545c9b86c79a0f1b56f4d3288f3fb532199b29ccea93960a791d66f`.
- e9tool SHA-256:
  `8569c9c62f2b9ad79f22903ae01b58d99abad438023f7a4d49538785419625d0`.
- e9patch SHA-256:
  `083e7deee709d66b82ca9e3692c7cd31326e64fdcec515704c769d336320d5fe`.
- Log level: default. Relaxations: none.

The Hermit timing runs used `--strict` without `--verify`, so they establish L1
for the measured invocation, not L2. The separate compatibility matrix covers
these programs at L2; this benchmark additionally checked equal output
fingerprints across native, ptrace, and e9patch warmups before timing.

## Method

Each mode executed the same program and arguments:

```text
native:  timeout 180 taskset -c 0 PROGRAM ARGS...
ptrace:  timeout 180 taskset -c 0 hermit run --strict -- PROGRAM ARGS...
e9patch: timeout 180 taskset -c 0 hermit run --backend e9patch --strict -- PROGRAM ARGS...
```

Fixtures were generated deterministically in the guest-visible checkout. Each
workload received one unmeasured warmup in each mode, followed by five measured
trials. Workload and backend order rotated between trials to distribute host
drift. The table reports the median wall time and ratios of those medians.
Elapsed time includes `timeout`, `taskset`, Hermit startup, guest execution, and
teardown. No run timed out or exited nonzero; all 150 timed samples completed.

The e9patch warmups confirmed instruction-map cache hits for every final run.
Rewritten rows also confirmed rewrite-cache hits. Consequently, this is the
steady-state cached cost; it does not include e9tool or e9patch preprocessing.

## Workloads

| Program | Fixed work | Version |
| --- | --- | --- |
| `gcc` | Compile generated C with 100 non-inlined arithmetic functions, `-O2` | GCC 11.5.0 |
| `g++` | Compile 100 generated C++ template instantiations, `-O2` | GCC 11.5.0 |
| `rustc` | Compile generated Rust with 100 non-inlined arithmetic functions, `-O` | 1.99.0-nightly |
| `make` | Force-build four C objects, 20 arithmetic functions each, `-j1` | GNU Make 4.3 |
| `python3` | 100,000 modular integer-arithmetic iterations | Python 3.9.25 |
| `node` | 100,000 modular BigInt iterations | Node 16.20.2 |
| `ruby` | 100,000 modular integer-arithmetic iterations | Ruby 3.0.7p220 |
| `gzip` | Compress 4 MiB deterministic pseudorandom input, `-9 -n` | gzip 1.12 |
| `xz` | Compress 512 KiB deterministic pseudorandom input, `-6` | XZ Utils 5.2.5 |
| BusyBox | SHA-256 of 8 MiB deterministic pseudorandom input | BusyBox 1.35.0 |

## Timings

Times are median seconds from five trials. `P/N` is ptrace / native, `E/N` is
e9patch / native, and `E/P` is e9patch / ptrace.

| Workload | e9patch sites | Native | Ptrace strict | e9patch strict | P/N | E/N | E/P |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| GCC compile | 28/28 | 0.341 | 12.731 | 12.803 | 37.35x | 37.56x | 1.006x |
| G++ compile | 28/28 | 0.391 | 18.683 | 18.430 | 47.84x | 47.19x | 0.986x |
| Rustc compile | 24/49 candidates | 0.726 | 17.609 | 17.648 | 24.24x | 24.30x | 1.002x |
| Make, four objects | zero-site | 0.270 | 11.205 | 11.352 | 41.56x | 42.11x | 1.013x |
| Python loop | zero-site | 0.051 | 9.402 | 9.324 | 185.77x | 184.23x | 0.992x |
| Node loop | zero-site | 0.189 | 16.313 | 16.112 | 86.49x | 85.42x | 0.988x |
| Ruby loop | zero-site | 0.023 | 0.250 | 0.253 | 10.84x | 10.97x | 1.012x |
| gzip, 4 MiB | zero-site | 0.170 | 9.218 | 9.232 | 54.32x | 54.40x | 1.001x |
| xz, 512 KiB | zero-site | 0.151 | 3.085 | 3.064 | 20.39x | 20.24x | 0.993x |
| BusyBox SHA-256, 8 MiB | 183/183 | 0.058 | 1.213 | 1.250 | 21.10x | 21.74x | 1.031x |

The largest e9patch-versus-ptrace median difference was BusyBox at +3.1%; the
smallest was G++ at -1.4%. With five samples on a shared host and no confidence
interval, these small mixed-direction differences should be treated as noise,
not backend wins or regressions.

## Interpretation and limits

1. Cached e9patch selection adds no material runtime overhead beyond ptrace in
   this corpus. It also does not bypass the shared Detcore runtime costs.
2. Cold-cache preprocessing can dominate first use, especially for large
   binaries, and needs a separate startup/preprocessing benchmark.
3. Only the main executable is preprocessed. Children spawned by compiler and
   Make workloads continue through the ptrace correctness path.
4. Native medians below one second amplify fixed startup cost. The strict/native
   columns quantify these exact probes, not long-running throughput.
5. The host is shared. CPU pinning, rotated order, warmups, and medians reduce
   noise but do not replace an isolated host, more samples, or confidence
   intervals.

Raw local evidence was written to `/tmp/e9perf-results-6cadacb0.csv`; generated
fixtures and raw logs are intentionally not versioned.
