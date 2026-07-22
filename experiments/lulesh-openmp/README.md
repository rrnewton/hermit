# LULESH OpenMP determinism

This experiment runs the DOE LULESH hydrodynamics proxy application with four
OpenMP threads under Hermit strict mode. It exercises barriers, reductions, and
parallel loops in a real scientific workload rather than a synthetic thread
fixture.

## Run

From the repository root:

```sh
./experiments/lulesh-openmp/run.sh
```

The runner clones LULESH 2.0.3 at commit
`46c2a1d6db9171f9637d79f407212e0f176e8194` into
`target/lulesh-openmp/source`, builds the non-MPI OpenMP variant with GCC, and
builds release Hermit. During the build it applies
`lulesh-instrumentation.patch`, then restores the pinned checkout immediately
after the instrumented binary is linked. The patch records the team size from
inside LULESH's `CalcCourantConstraintForElems` parallel region and writes a
lossless `%a` snapshot of every persistent node- and element-centered numerical
field plus the timestep scalars. The runner also verifies that the guest links
`libgomp`. Generated source, objects, binaries, and state snapshots remain under
`target/` and are not committed.

Use an existing pinned checkout and binary with:

```sh
./experiments/lulesh-openmp/run.sh \
  --skip-build \
  --source /path/to/LULESH \
  --hermit ./target/release/hermit \
  --output /tmp/lulesh-evidence
```

The default workload uses mesh size 10, 10 cycles, four OpenMP threads, two
runs, and a 180-second timeout per run. Values below two for `--runs` are
rejected because a single observation cannot establish repeatability. Each
execution is equivalent to:

```text
hermit --log=error run --strict --base-env=minimal \
  --env=LC_ALL=C --env=OMP_NUM_THREADS=4 --env=OMP_DYNAMIC=false \
  --env=LULESH_STATE_FILE=/tmp/lulesh-state-RUN.txt \
  --tmp=<LULESH checkout> -- /tmp/lulesh2.0 -s 10 -i 10
```

The `error` log threshold prevents wall-clock timestamps from diagnostic logs
from contaminating the observation. Strict scheduling, virtual time, and PMU
preemption remain enabled. The runner requires every execution to exit zero;
report exactly one requested thread count, observed team size, positive
parallel-region count, requested iteration count, and finite final origin
energy; and write a state snapshot with the expected mesh dimensions. All
checks are anchored full-line matches, so values such as 4/40 or 1/10 cannot
alias. It then compares complete stdout, stderr, state, and exit status
byte-for-byte and includes all four in the SHA-256 observation fingerprint. The
local `.gitattributes` preserves LULESH raw stdout whitespace as part of that
exact observation.

## Recorded result

`evidence_20260721/` was collected from Hermit commit
`d6438c9d5fe1b2076eab3d563b445b96c15f70d7` on an x86-64 AMD EPYC 9D85 host
with GCC 11.5.0 and `/lib64/libgomp.so.1`.

Both strict-mode runs exited zero with empty stderr, observed an actual
four-thread OpenMP team in 110 instrumented parallel regions, and produced the
same full-state SHA-256:

```text
99692f4cb51fdd7384dce3b8a41bfec6a29532361381085e77c44d415875a433
```

The combined observation fingerprint, which also includes exit status and the
complete stdout and stderr streams, is:

```text
645471f4ad73fb743fb5f1b8b55e0eb8fb0f383a3d24313201cfedbc713391e4
```

The output reports 10 iterations, final origin energy `2.596764e+05`, and
identical numerical validation residuals. The exact virtual elapsed time, grind
time, and figure of merit also match. The committed manifest records the state
hashes; the two identical 345,760-byte generated snapshots are omitted from Git.

This establishes repeatability for this pinned workload and host. It is not a
proof for every LULESH input, compiler, OpenMP runtime, or PMU implementation.
