# `examples/` backend characterization

Measured 2026-08-17 UTC (2026-08-16 PDT) for task
`ov-examples-ratchet`. This report preserves research evidence only. The
measurement did not change a manifest, `ci/expected-e2e-plan.json`, the
scorecard, or any backend enablement.

## Command and oracle

Every backend result used the manifest's exact guest shape: direct argv for
`timed-progress-bar.py` and `rand.py`, and `sh -c ./examples/<name>.sh` for the
three string-form manifest entries. The Hermit command was:

```text
hermit --log=info run --backend BACKEND --strict --verify \
  --verify-json REPORT --keep-logs --verify-log-dir LOGS -- GUEST
```

No `--no-virtualize-cpuid`, `--max-timeslice=disabled`, or other relaxation was
passed. "Canonical L2" below requires `verified=true`, `verdict=matched`,
`bitwise_parity=true`, canonical log comparison, and nonzero INFO counts.
KVM's matched output/exit result has `compare_logs=false`, `0/0` INFO, and is
not L2. SaBRe additionally requires two complete, eligible path records.

## Binaries

| Used for | Binary SHA-256 | Embedded version | Included/runtime evidence |
|---|---|---|---|
| ptrace, KVM | `46e7ff01a50cc979ce5a1fd4e58d9a04730fa638ecca4881bb3a797caa853244` | `g0f8516f171ee-dirty` | ptrace and KVM executed. DBT and SaBRe explicitly reported that support was not included. LiteInst code was present, but this binary had no staged preload DSO. |
| SaBRe, LiteInst | `203b7e9435b292a322529c4286dc6e64083a3d0abc44e98dfa102b97685544d2` | `g79cc84087904-dirty` | Built with `sabre`; staged `sabre`, `libdetcore_sabre.so`, and `libreverie_liteinst.so`. DBT explicitly reported that support was not included. |
| DBT | `0f8f5c42e90cdf56013171e7b62855817f8ec8cc4cc1600a5d575166ea060d33` | `g79cc84087904-dirty` | Built with `dbt` against a read-only preserved copy of the owner's uncommitted Reverie DBT evidence work. Protected framed evidence passed a `/bin/true` smoke at 80/80 INFO. SaBRe explicitly reported that support was not included. This is not clean, reproducible commit provenance. |

## Results

| Example | Naked control, 3 runs | ptrace | KVM | DBT | SaBRe | LiteInst |
|---|---|---|---|---|---|---|
| `timed-progress-bar.py` | Same final-output hash 3/3; output alone cannot demonstrate determinization | Canonical L2, 104104/104104 INFO, 16.64s | Output/exit matched, 0/0 INFO, 30.81s; not L2 | Canonical L2, 3408/3408 INFO, 12.42s | Canonical L2, 202871/202871 INFO, 2/2 eligible path records, 0 fallback/trusted sites, 90.25s | Canonical divergence, 104307/104305 INFO, 6.25s; virtual-time differences around `/proc/self/maps` and trampoline setup |
| `rand.py` | Three distinct output hashes | Canonical L2, 3332/3332 INFO, 1.70s | Output/exit matched, 0/0 INFO, 1.12s; not L2 | Canonical divergence, 3276/3274 INFO, 11.92s; run 1 had two extra terminal scheduler messages | Canonical L2, 3214/3214 INFO, 2/2 eligible path records, 0 fallback/trusted sites, 2.41s | Canonical divergence, 4332/4332 INFO, 1.34s; virtual-time differences during LiteInst setup |
| `race.sh` via `sh -c` | Three distinct output hashes | Canonical L2, 3543/3543 INFO, 0.68s | Output/exit matched, 0/0 INFO, 1.35s; not L2 | No result, 2.61s: protected evidence missing image FINAL frames | Comparator matched canonically, 3427/3427 INFO, but 66 trusted shared-object sites across 2 runs; path ineligible, not L2 | No result, 0.14s: required preload runtime cannot survive post-start exec |
| `date.sh` via `sh -c` | Three distinct output hashes | Canonical L2, 683/683 INFO, 0.15s | Output/exit matched, 0/0 INFO, 0.55s; not L2 | No result, 1.31s: protected evidence missing image FINAL frames | Comparator matched canonically, 410/410 INFO, but 66 trusted shared-object sites across 2 runs; path ineligible, not L2 | No result, 0.15s: required preload runtime cannot survive post-start exec |
| `devrand.sh` via `sh -c` | Three distinct output hashes | Canonical L2, 798/798 INFO, 0.17s | Output/exit matched, 0/0 INFO, 0.59s; not L2 | No result, 1.28s: protected evidence missing image FINAL frames | Comparator matched canonically, 504/504 INFO, but 66 trusted shared-object sites across 2 runs; path ineligible, not L2 | No result, 0.13s: required preload runtime cannot survive post-start exec |

The naked controls are the clearest reason not to substitute output parity for
the canonical predicate. Four examples varied in all three naked runs, while
the progress bar's final output was identical by construction. Its stable
output therefore proves nothing about whether its internal execution was
deterministic. KVM reported the same output/exit match for all five examples,
but compared no INFO evidence.

## Owner-review promotion candidates

These eight verify cells produced no-relaxation canonical, path-eligible
results and are absent from the current expected plan:

1. `applications/example-timed-progress-bar` — ptrace
2. `determinism-stress/example-race` — ptrace
3. `language-runtimes/example-python-random` — ptrace
4. `system-utils/example-date` — ptrace
5. `system-utils/example-devrand` — ptrace
6. `applications/example-timed-progress-bar` — DBT
7. `applications/example-timed-progress-bar` — SaBRe
8. `language-runtimes/example-python-random` — SaBRe

They are candidates, not promotions. Most have one no-relaxation verification
invocation, where each invocation compares two executions. The owner must set
the repeat bar and authorize any expected-plan or manifest change.

KVM's five matches must not roll into green because their oracle is output/exit
only. The three SaBRe shell cells must not roll into green because their path
evidence is ineligible. DBT `rand.py`, all three DBT shell cells, and all five
LiteInst cells failed the canonical or no-result gate.

## LiteInst tightening result

The earlier portable measurements passed `--no-virtualize-cpuid` and
`--max-timeslice=disabled`. Under those relaxations, LiteInst reported canonical
matches for the progress bar at 4467/4467 INFO and `rand.py` at 4326/4326 INFO.
Those passes did not survive the no-relaxation run:

- progress bar: canonical divergence at 104307/104305 INFO;
- `rand.py`: canonical divergence at 4332/4332 INFO;
- `race.sh`, `date.sh`, and `devrand.sh`: no result because the preload runtime
  cannot survive the manifest's post-start exec.

This is the stricter predicate working as intended: the relaxed passes are not
promotion evidence.

## Raw evidence retained in the measurement slot

The tracked report above is self-contained because slot-local artifacts may be
reclaimed. At measurement time, the complete JSON receipts, logs, commands,
statuses, and timings were retained under `ignored/examples-ratchet/`:

- `no-relax-ptrace-20260817/` and
  `no-relax-ptrace-manifest-exact-20260817/`;
- `no-relax-kvm-20260817/` and
  `no-relax-kvm-manifest-exact-20260817/`;
- `dbt-examples-20260817/`, `dbt-manifest-exact-20260817/`, and `dbt-smoke/`;
- `no-relax-sabre-20260817/` and
  `no-relax-sabre-manifest-exact-20260817/`;
- `no-relax-liteinst-staged-20260817/` and
  `no-relax-liteinst-manifest-exact-20260817/`;
- `naked-controls-20260817/`.

The excluded `no-relax-liteinst-20260817/` attempt used a binary without a
staged preload DSO. It was an environmental no-result, not a LiteInst product
result.
