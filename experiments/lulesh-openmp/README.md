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
builds release Hermit. It verifies that the guest links `libgomp` before
running. Generated source, objects, and binaries remain under `target/` and are
not committed.

Use an existing pinned checkout and binary with:

```sh
./experiments/lulesh-openmp/run.sh \
  --skip-build \
  --source /path/to/LULESH \
  --hermit ./target/release/hermit \
  --output /tmp/lulesh-evidence
```

The default workload uses mesh size 10, 10 cycles, four OpenMP threads, two
runs, and a 180-second timeout per run. Each execution is equivalent to:

```text
hermit --log=error run --strict --base-env=minimal \
  --env=LC_ALL=C --env=OMP_NUM_THREADS=4 --env=OMP_DYNAMIC=false \
  --tmp=<LULESH checkout> -- /tmp/lulesh2.0 -s 10 -i 10
```

The `error` log threshold prevents wall-clock timestamps from diagnostic logs
from contaminating the observation. Strict scheduling, virtual time, and PMU
preemption remain enabled. The runner requires every execution to exit zero,
report the requested OpenMP thread and iteration counts, and emit a final
origin energy. It then compares complete stdout, stderr, and exit status
byte-for-byte. The local `.gitattributes` preserves LULESH raw stdout
whitespace as part of that exact observation.

## Recorded result

`evidence_20260721/` was collected from Hermit main commit
`cdce9cfc1447ff5edf8ebf306dc36ece0c4ad06c` on an x86-64 AMD EPYC 9D85 host
with GCC 11.5.0 and `/lib64/libgomp.so.1`.

Both strict-mode runs exited zero with empty stderr and the same observation
fingerprint:

```text
920d92b23587b20ce395869d5f8aacfb30df6f9fe011ffe8c7f3d7660a3374e6
```

The output reports four threads, 10 iterations, final origin energy
`2.596764e+05`, and identical numerical validation residuals. The exact virtual
elapsed time, grind time, and figure of merit also match. A separate five-run
follow-up produced the same fingerprint in all five runs.

This establishes repeatability for this pinned workload and host. It is not a
proof for every LULESH input, compiler, OpenMP runtime, or PMU implementation.
