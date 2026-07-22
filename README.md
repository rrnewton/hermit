# Hermit: Reproducible Linux Execution

Hermit runs unmodified x86-64 Linux programs under the
[Reverie](https://github.com/facebookexperimental/reverie) ptrace backend. It
controls common sources of nondeterminism, including thread scheduling, time,
random data, CPUID results, address layout, and selected file metadata.

This walkthrough demonstrates four working workflows:

1. repeat an execution with stable guest-visible inputs;
2. record an execution and replay it, with or without GDB;
3. search seeded thread schedules for a concurrency failure; and
4. bisect two schedules to identify the events that change the outcome.

> [!WARNING]
>
> Hermit is in maintenance mode. It is not a security boundary, and it does not
> make changing files or external network responses deterministic. Record/replay
> support is experimental and narrower than `hermit run` compatibility.

## Requirements

Use an x86-64 Linux host with Rust nightly (selected by
`rust-toolchain.toml`), libunwind and LZMA development libraries, Linux user/PID
namespaces, and parent-child ptrace and seccomp support. GDB is needed for the
debugger section, and the Python demo uses `/usr/bin/python3`. The
final schedule-bisection demo also needs user-accessible CPU performance
counters.

Run every command below from the repository root in the same Bash session.
The commands use private temporary and ignored build-artifact directories; they
require no external network access.

## Quick Build

Build the optimized workspace, then build the debug binaries used for this
walkthrough and establish their paths:

```bash
cargo build --release
cargo build
export HERMIT="$PWD/target/debug/hermit"
export HELLO_RACE="$PWD/target/debug/hello_race"
export HEAP_PTRS="$PWD/target/debug/rustbin_heap_ptrs"
export DEMO_TMP="$(mktemp -d -t hermit-demo.XXXXXX)"
export DEMO_ARTIFACTS="$PWD/target/${DEMO_TMP##*/}"
mkdir -p "$DEMO_ARTIFACTS"
test -x "$HERMIT" && test -x "$HELLO_RACE" && test -x "$HEAP_PTRS"
"$HERMIT" --version
```

The walkthrough uses debug artifacts for the validated record/replay path and
source-resolved analyzer output; the release build remains available for normal
use.

These demos explicitly disable CPUID virtualization and PMU timer preemption
so that the short examples also work on hosts without those features. CPUID is
therefore a host input in these commands, and CPU-bound guests receive fewer
preemption opportunities.

```bash
run_hermit() {
  "$HERMIT" --log=error run \
    --base-env=minimal \
    --no-virtualize-cpuid \
    --preemption-timeout=disabled \
    "$@"
}
```

## Deterministic Run

### Basic Execution And Virtual Inputs

Hermit preserves the guest's exit status and output:

```bash
run_hermit -- /bin/echo hello
```

Random bytes and wall-clock time are virtual guest inputs. Repeating either
command produces identical output when the executable, arguments, inputs, and
Hermit configuration are unchanged:

```bash
for attempt in 1 2; do
  run_hermit -- /bin/sh -c 'od -An -N8 -tx1 /dev/urandom'
done

for attempt in 1 2; do
  run_hermit -- /bin/date +%s.%N
done
```

### Python Entropy And Hash Ordering

This program observes three process-level entropy sources: random bytes,
Python's randomized string hash, and hash-set iteration order. Native processes
normally differ; the two Hermit executions match exactly.

```bash
export PYTHON="/usr/bin/python3"
export PYTHON_DEMO='import os; print("random="+os.urandom(16).hex()); print("hash="+str(hash("hermit-demo"))); print("set="+",".join(set(["alpha","beta","gamma","delta","epsilon"])))'

for attempt in 1 2; do
  "$PYTHON" -c "$PYTHON_DEMO"
done

for attempt in 1 2; do
  run_hermit -- "$PYTHON" -c "$PYTHON_DEMO" | tee "$DEMO_TMP/python-hermit-$attempt.txt"
done
cmp "$DEMO_TMP/python-hermit-1.txt" "$DEMO_TMP/python-hermit-2.txt"
```

### Address Layout And Built-In Verification

The heap-pointer guest shows stable addresses across separate Hermit runs:

```bash
for attempt in 1 2; do
  run_hermit -- "$HEAP_PTRS" | tee "$DEMO_TMP/heap-hermit-$attempt.txt"
done
cmp "$DEMO_TMP/heap-hermit-1.txt" "$DEMO_TMP/heap-hermit-2.txt"
```

`--verify` runs the guest twice and compares status, output, and Hermit's
deterministic execution log:

```bash
run_hermit --verify -- /bin/echo reproducible
```

The guest must be idempotent. A first run that changes a file, database, cache,
or external service can legitimately change the second run.

## Record And Replay

Create an isolated recording directory, record `/bin/echo`, inspect the
recording, and replay it to completion:

```bash
export DEMO_DATA_DIR="$DEMO_TMP/recordings"
mkdir -p "$DEMO_DATA_DIR"
"$HERMIT" --log=error record start \
  --data-dir="$DEMO_DATA_DIR" -- /bin/echo recorded
"$HERMIT" record list --data-dir="$DEMO_DATA_DIR"
"$HERMIT" record list --json --data-dir="$DEMO_DATA_DIR"
"$HERMIT" --log=error replay --autopilot --data-dir="$DEMO_DATA_DIR"
```

Hermit can also record and immediately verify a replay. This form deletes its
temporary recording after a successful match:

```bash
"$HERMIT" --log=error record start --verify \
  --data-dir="$DEMO_TMP/verified-recording" -- /bin/echo verified-recording
```

### Replay Under GDB

Without `--autopilot`, `hermit replay` starts a replay gdbserver and GDB client.
The following noninteractive session connects, stops at the loader entry,
continues the guest, and exits after `/bin/echo` completes:

```bash
timeout 90 "$HERMIT" --log=error replay \
  --data-dir="$DEMO_DATA_DIR" \
  --gdbex='set confirm off' \
  --gdbex='set pagination off' \
  --gdbex=continue
```

For an interactive debugging session, omit the three `--gdbex` options and the
external timeout. Keep the recording directory, executable, inputs, and Hermit
revision unchanged between recording and replay.

## Chaos Concurrency Testing

`hello_race` contains an intentional data race. Chaos mode makes scheduler
choices with a seeded PRNG, so different seeds explore different interleavings
and the same seed reproduces the same result.

```bash
chaos_run() {
  local seed="$1"
  "$HERMIT" --log=error run \
    --chaos \
    --seed="$seed" \
    --base-env=minimal \
    --no-virtualize-cpuid \
    --preemption-timeout=disabled \
    --env=HERMIT_MODE=chaos \
    -- "$HELLO_RACE"
}
```

Seed 1 passes. Seed 0 reaches the antagonistic schedule and returns the guest's
expected failure status; the shell assertion turns that expected failure into
a successful demo step.

```bash
chaos_run 1

if chaos_run 0; then
  echo 'unexpected pass for seed 0' >&2
  exit 1
else
  echo 'seed 0 reproduced the expected concurrency failure'
fi
```

Surveying a small seed range finds both outcomes while retaining each run's
output for inspection:

```bash
for seed in $(seq 0 15); do
  if chaos_run "$seed" >"$DEMO_TMP/chaos-$seed.txt"; then
    result=pass
  else
    result=fail
  fi
  printf 'seed=%s result=%s\n' "$seed" "$result"
done
```

### Save And Replay A Failing Schedule

A schedule artifact reproduces the exact observed failure without relying only
on the seed. Both commands are expected to return the guest's failure status.

```bash
export CHAOS_SCHEDULE="$DEMO_ARTIFACTS/hello-race-schedule.json"

if "$HERMIT" --log=error run \
  --chaos --seed=0 \
  --base-env=minimal \
  --no-virtualize-cpuid \
  --preemption-timeout=disabled \
  --env=HERMIT_MODE=chaos \
  --record-preemptions-to="$CHAOS_SCHEDULE" \
  -- "$HELLO_RACE" >"$DEMO_TMP/chaos-recorded.txt"; then
  echo 'unexpected pass while recording the failing schedule' >&2
  exit 1
fi
test -s "$CHAOS_SCHEDULE"

if "$HERMIT" --log=error run \
  --chaos \
  --base-env=minimal \
  --no-virtualize-cpuid \
  --preemption-timeout=disabled \
  --env=HERMIT_MODE=chaos \
  --replay-preemptions-from="$CHAOS_SCHEDULE" \
  -- "$HELLO_RACE" >"$DEMO_TMP/chaos-replayed.txt"; then
  echo 'unexpected pass while replaying the failing schedule' >&2
  exit 1
fi
cmp "$DEMO_TMP/chaos-recorded.txt" "$DEMO_TMP/chaos-replayed.txt"
```

## Schedule Bisection

`hermit analyze` first finds passing and failing schedules, then bisects their
event streams to identify the ordering that changes the outcome. Build a debug
copy of the guest so the final report can resolve source locations:

```bash
cargo build -p hermetic_infra_hermit_flaky-tests --bin hello_race
export HELLO_RACE_DEBUG="$PWD/target/debug/hello_race"
export ANALYSIS_REPORT="$DEMO_ARTIFACTS/hello-race-analysis.json"
```

The analysis is intentionally the slow finale: it runs the guest many times,
requires PMU access, and can emit scheduler-desynchronization diagnostics while
converging. A successful run ends with `Completed analysis successfully`.

```bash
timeout 600 "$HERMIT" analyze \
  --run-arg=--base-env=host \
  --report-file="$ANALYSIS_REPORT" \
  --analyze-seed=0 \
  --search -- \
  --chaos --summary --preemption-timeout=400000 -- \
  "$HELLO_RACE_DEBUG"

"$PYTHON" -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["header"]); print("critical events:", d["critical_event1"]["event_index"], d["critical_event2"]["event_index"])' "$ANALYSIS_REPORT"
```

On the verified host, the report identified two adjacent events in different
`hello_race` threads and resolved both stacks to the intentional racy access in
`flaky-tests/hello_race.rs`. Event numbers can vary with the binary and Hermit
revision; the source-level diagnosis is the durable result.

## Current High-Water Mark

The following numbers describe the tested `rrnewton/hermit` main line used for
this walkthrough. They are scoped evidence, not universal compatibility claims.

- **Public Cargo inventory:** 333 discoverable tests, with 319 runnable by
  default and 14 explicitly ignored slow or PMU-sensitive tests. The largest
  packages are `hermit` (158 tests) and `detcore` (105 tests).
- **Internal integration matrix:** Meta's Buck configuration contains more
  than 700 guest/mode/rr combinations. It has not been fully ported to Cargo.
- **Run-mode launch coverage:** static and dynamic ELF programs, shell, Python,
  Node.js, OpenJDK, Go, curl, wget, Git, GCC, Make, direct Cargo, SQLite, and a
  multithreaded signal workload have passed bounded probes.
- **Functional application coverage:** a curl/Python loopback HTTP workflow
  completed three out of three strict-mode runs with identical output and
  status hashes. Git, nginx, and Redis functional experiments timed out
  repeatably and are not claimed as supported workflows.
- **Record/replay coverage:** exact record/replay succeeded for the checked
  `echo`, `ls`, `cat`, `grep`, `sort`, and `wc` fixtures. More complex
  subprocess, filesystem, network, JVM, and Node.js cases remain limited.

See the [arbitrary-binary matrix](ai_docs/arbitrary-binary-matrix.md),
[record/replay experiment](experiments/record-replay-matrix_20260721/README.md),
and [wave-three application experiment](experiments/arbitrary-binary-wave3/README.md)
for commands, host metadata, hashes, and failure details.

## Scope And Next Steps

- Keep file contents and mount layouts fixed, prefer a minimal environment,
  and avoid external networking when asserting reproducibility.
- Use PMU timer preemption when exploring CPU-bound races. The portable chaos
  commands above still find this syscall-rich demo failure without it.
- Treat version probes as launch coverage, not proof that every workflow of a
  program works.
- Benchmark the real workload; ptrace overhead varies with syscall frequency,
  thread count, scheduling, and logging.

For full option and troubleshooting coverage, continue with the
[User Guide](docs/USER_GUIDE.md), [Architecture](docs/ARCHITECTURE.md), and
[Error Catalog](docs/ERROR_CATALOG.md). Hermit is BSD-licensed; see
[LICENSE](LICENSE).
