# `examples/` — runnable dagrun DAGs

Eight DAGs demonstrate the runner's core ideas. The
first six use `sleep`/`echo`/pure shell and finish in a few seconds. Example 07 is an intentionally
CPU-intensive, standard-library-only Python scaling benchmark; start with its reduced-work smoke
command before spending a larger target allowance. Example 08 is a real, comparatively expensive
clean-build benchmark with Linux/toolchain prerequisites. Each example also carries `description`
fields (a top-level one for the whole DAG and one per node) — free-form documentation that never
affects scheduling.

Two of them (`02-diamond` and `04-memory-aware`) additionally ship a **side-by-side YAML edition**
(`.yaml`) that loads to the *exact same DAG* as its `.json` twin — a "literate" version with inline
comments and multi-line block-scalar descriptions. `--dag` auto-detects the format by extension
(`.yaml`/`.yml` → YAML, else JSON), so you can point `run`/`list`/`ascii`/`dot`/`json`/`yaml` at
either file. See `02-diamond.yaml` for what a literate DAG looks like.

Run an example with the installed command:

```sh
dagrun run --dag examples/01-linear-chain.json --allow-cgroup-failure
```

From a source checkout, run `./setup` first and use `./bin/dagrun` in place of the installed
command. The repository's behavioral differential runs the same examples against both
implementations.

**Why `--allow-cgroup-failure`?** Boxing each step in its own Linux cgroup-v2 sandbox is this
tool's whole point, so `run` **boxes by default**: it re-execs the run inside a transient
`systemd-run --user --scope` and caps each step. On a machine without cgroup-v2 + a working systemd
`--user` scope (many laptops, most CI runners, macOS), a bare `run` would instead **error with exit
code 3** rather than silently run unprotected. `--allow-cgroup-failure` opts out of the requirement:
it runs the steps **un-boxed** (with a visible warning) so these demo DAGs work out-of-the-box
anywhere. Drop the flag on a Linux host with a systemd user session to get the real per-step boxing.
The commands below include it so they are copy-paste runnable on any machine.

Before running, it is worth *looking* at each graph — every example works with `list`, `ascii`, `dot`,
`json`, and `yaml` too:

```sh
dagrun list  --dag examples/02-diamond.json   # one line per step, with class + deps
dagrun ascii --dag examples/02-diamond.json   # topological-layer view
dagrun dot   --dag examples/02-diamond.json | dot -Tsvg -o dag.svg   # a picture
```

Work through them in order — each adds one concept on top of the previous.

## 1. `01-linear-chain.json` — a trivial linear chain

`build.compile → test.unit → package.tarball`: the simplest possible pipeline, three steps in a
straight line where each waits for the previous one. Start here to see the basic run output (a
`▶ START` / `✓ PASS` line per step and the final `PASS` summary). Because each step depends on the one
before it, nothing runs in parallel and the wall time is the sum of the steps (~6s).

```sh
dagrun run --dag examples/01-linear-chain.json --allow-cgroup-failure
```

## 2. `02-diamond.json` — a diamond with parallel branches

`build.app` fans out to two independent checks (`test.unit` and `test.lint`) that run **at the same
time**, and `deploy.staging` waits for **both** to finish. This is the first example where you see
concurrency: the two middle steps overlap, so the wall time is shorter than their sum. `test.unit`
carries a larger `est_duration_s`, so when both become ready the runner dispatches it first
(longest-processing-time ordering). If either check failed, `deploy.staging` would be reported as
`skipped`.

```sh
dagrun run --dag examples/02-diamond.json --allow-cgroup-failure
dagrun ascii --dag examples/02-diamond.json   # shows the 3 topological layers
```

This example also ships as **`02-diamond.yaml`** — the same DAG, written literately: inline `#`
comments and multi-line block-scalar (`|-` / `>-`) descriptions. It loads identically (proven by
`cross/differential.py`), so you can run the YAML edition anywhere the JSON one runs:

```sh
dagrun run  --dag examples/02-diamond.yaml --allow-cgroup-failure
dagrun json --dag examples/02-diamond.yaml   # YAML in, canonical JSON out
```

## 3. `03-scarce-resource-browser.json` — a named scarce resource

Two end-to-end steps (`e2e.login`, `e2e.checkout`) each declare `"resources": {"browser": 1}`, and the
top-level `"resource_caps": {"browser": 1}` allows only **one** browser step to run at a time. So the
two e2e steps run one-after-another even though both are ready — while the ordinary `test.unit` step
(which needs no browser) runs alongside whichever e2e step currently holds the browser.

`browser` here is just a **name you chose** — the caps are arbitrary caller-defined strings, not a
built-in. It can serialize browser end-to-end tests that share fixed ports or a display. You could
equally cap `"gpu"`, `"db"`, or `"licenses"`.

```sh
dagrun run --dag examples/03-scarce-resource-browser.json --allow-cgroup-failure
```

## 4. `04-memory-aware.json` — memory hints bound active DAG steps

Each step carries a memory hint: `build.compile` and `test.integration` set `rss_baseline_bytes`
(estimated peak RSS), and `test.fuzz` additionally pins a `hard_mem_max_bytes` cap that overrides its
baseline. With those hints, `--max-mem` derives the largest `--max-steps` ceiling whose modeled
worst-case footprint fits a RAM budget, instead of you hard-coding a number:

```sh
dagrun run --dag examples/04-memory-aware.json --max-mem 4G --allow-cgroup-failure   # -> --max-steps 1 (worst case 4 GiB)
dagrun run --dag examples/04-memory-aware.json --max-mem 8G --allow-cgroup-failure   # -> --max-steps 2 (worst case 7 GiB)
```

A 4 GiB budget only fits one step at a time; 8 GiB fits the two largest that can co-run; a large
enough budget lets all three run at once when `--max-steps` permits it. `--max-cpus` separately caps
each runner-controlled step width, while a boxed run's outer quota arbitrates their aggregate CPU
bandwidth. Without `--max-mem`, `--max-steps` defaults to the effective CPU target. (This example
sets `mem_cap_factor: 1.0`, `mem_cap_floor_bytes: 0`, and `outer_mem_safety_factor: 1.0` so the
modeled numbers are exactly the byte values above; the defaults add headroom.)

This example also ships as **`04-memory-aware.yaml`** — the same DAG in literate YAML (byte-value
comments on each memory hint), loading identically to the JSON:

```sh
dagrun run --dag examples/04-memory-aware.yaml --max-mem 8G --allow-cgroup-failure
```

## 5. `05-inner-jobs.json` — a step with internal parallelism

`build.app` runs its *own* parallel build (imagine `make -j8`). Declaring
`"preferred_inner_jobs": 8` tells the runner how wide the step is internally, so it can set an inner
CPU cap when cgroup boxing is active, check that width against the run's per-step `--max-cpus`
ceiling, and account for the extra memory a wide `cpu-bound` step uses in the budget model. Other
legal steps may overlap it even when their requested widths sum above `--max-cpus`; the boxed outer
quota arbitrates the shared CPU bandwidth.

By **default** the runner also *appends* an inner-jobs flag to your command derived from that width —
the default template is `-j`, so a `cmd` of `make build` would be run as `make build -j 8`. This step
already hardcodes its own `make -j8`, so `"jobs_flag": ""` declares a **self-managed fixed width**.
That is allowed while the run budget is at least eight, but `--max-cpus 4` refuses before spawning:
the runner cannot honestly rewrite the command to four workers. Such a step also cannot be swept.

Prefer a command that accepts a runner-controlled template such as `"-j%d"`, `"--jobs="`, or
`"--num-threads"`. Then a smaller `--max-cpus` caps the appended flag, scheduler accounting,
planner recommendation, and child `cpu.max` together.

```sh
# This self-managed fixture declares a fixed width of eight, so make the required budget explicit.
dagrun run --dag examples/05-inner-jobs.json --max-cpus 8 --allow-cgroup-failure

# Demonstrate the fail-closed case (exits 2 before the hardcoded -j8 command starts):
dagrun run --dag examples/05-inner-jobs.json --max-cpus 4 --allow-cgroup-failure
```

## 6. `06-step-sweep.json` — profile & experiment with individual steps

This example exists to demonstrate the **per-step profiling tools**. Its `build.app` step is a small
CPU-bound workload that *actually scales* with its inner parallelism: it splits a fixed amount of
pure-shell work across N workers, where N is the inner-jobs width the runner passes in via the step's
`jobs_flag`. So a `sweep` of it produces a real speedup curve. `test.unit` and `package.tarball` are
quick downstream steps so dependency-aware `--selected` is meaningful.

```sh
# Run one step and its build dependency:
dagrun run --dag examples/06-step-sweep.json --selected test.unit --allow-cgroup-failure

# Run only the named step when its build output is already present:
dagrun run --dag examples/06-step-sweep.json --selected test.unit --ignore-selected-deps --allow-cgroup-failure

# Run the whole DAG and print a per-step profile table afterwards:
dagrun run --dag examples/06-step-sweep.json --profile --allow-cgroup-failure

# Parallel-speedup study of the build step at inner -j1..-j8:
dagrun sweep --dag examples/06-step-sweep.json --step build.app --jobs 1..8 --allow-cgroup-failure
```

Every `run` and `sweep` above **auto-logs** resource-usage CSVs to the default profile store,
`./.dagrun/profiles/` (relative to your current directory) — you do not need `--perf-dir`. The
tool prints exactly where it appended. Drop `--allow-cgroup-failure` on a Linux host with a
systemd user session to get real per-step boxing, which also fills in the `peak_bytes` (peak memory)
column from each step's cgroup. A successful sweep also writes the standalone interactive site
`./.dagrun/profiles/profile_report.html`. Override the sweep artifact location with
`--output-dir DIR` (`--perf-dir` remains an alias) or
`$DAGRUN_PROFILE_DIR`, or turn logging off with `--no-profile`. Consider gitignoring `./.dagrun/`.

## 7. `07-graph-scaling-sweep.yaml` — target-time scaling across a DAG

This three-node chain exercises the graph-wide sweep rather than one hand-picked step:

- `scale.parallel` divides a fixed amount of work across every requested worker and should scale
  close to linearly before process overhead dominates;
- `scale.four-core` uses at most four workers, so its useful curve should plateau near four even
  when the sweep offers much wider settings; and
- `scale.sequential` performs the useful work on one worker, then makes extra requested workers do
  interfering work. Wider settings should increase CPU work and memory without improving the
  useful serial wall time.

All three commands accept `--jobs N` through the `generic-with-flag` cmdtype and its explicit
`jobs_flag`. The nodes are chained so stable topological order is visible, but the sweep clears each
node's dependency edges for its individual measurement and runs no second DAG node beside it.

Begin with a cheap mandatory-pass smoke run. A zero target still completes pass 1, and the explicit
list keeps that pass small:

```sh
DAGRUN_SYNTH_WORK=1000000 dagrun sweep \
  --dag examples/07-graph-scaling-sweep.yaml \
  --target-time 0 --jobs 1,2,4 --allow-cgroup-failure --no-profile
```

The smoke command disables profile writes because its reduced work size is a different workload;
mixing those rows into the full benchmark's model would make the dataset internally inconsistent.

Then collect a denser dataset under real cgroup boxing on a suitable Linux host:

```sh
dagrun sweep --dag examples/07-graph-scaling-sweep.yaml --target-time 10m
```

Without `--jobs`, pass 1 uses powers of two through physical cores, then exact physical-core and
logical-thread counts. Every later pass reruns its cumulative grid and inserts integer midpoints in
all remaining gaps. The allowance is checked only between passes: no pass is killed after it starts,
so the first pass can overrun a small target and reports by how much. Use `--step scale.four-core`
to isolate one node, or `--repeat K` to increase replication within each pass.
The fixture is intentionally repeatable; real commands used this way must likewise redo equivalent
work at every width instead of becoming incremental/no-op runs after their first invocation.

The raw rows land in `./.dagrun/profiles/` by default and carry sweep/pass/repeat/width provenance,
CPU work, wall time, and width-specific memory peaks. The fitted model remains outside the authored
YAML and can be rebuilt from those CSVs; each successful profiling-enabled sweep
refreshes the machine/container sidecar named
`scaling_model_<machine_id>_<container_class>.json`. Command-shape digests keep identified older
workloads out of the current curve and are retained as separate reservoirs in portable summaries.
Open `./.dagrun/profiles/profile_report.html` to explore the DAG and drill into its historical
speedup, memory, CPU-efficiency, and optional interval traces. Use `--report-html FILE` to choose
another path or `--no-report` to skip the HTML refresh.

## 8. `08-dagrun-clean-build-sweep.yaml` — a real clean-build scaling sweep

This one-node graph measures how the release build of the `dagrun` binary responds to the worker
count supplied by the `cargo-build` cmdtype. Every sample creates a fresh target directory, disables
incremental compilation, and runs locked and offline. The caller's downloaded registry/source cache
stays warm, while neither the repository target nor another sample's compiled artifacts can turn a
later width into an incremental build.

The fixture is intentionally Linux/x86_64-specific and expects its Rust dependencies to be cached
already. It also requires a pre-existing `DAGRUN_CARGO_SWEEP_ROOT`; the fresh per-sample target
directories below it are retained for inspection and caller-managed cleanup. Run it only under real
cgroup-v2 containment so both width enforcement and the optional time series are attributable:

```sh
study_root=$(mktemp -d /tmp/dagrun-cargo-build-sweep.XXXXXXXX)
mkdir -p "$study_root/targets" "$study_root/profiles"
DAGRUN_CARGO_SWEEP_ROOT="$study_root/targets" dagrun sweep \
  --dag examples/08-dagrun-clean-build-sweep.yaml \
  --step build.dagrun \
  --target-time 0 \
  --repeat 3 \
  --profile-timeseries 250ms \
  --output-dir "$study_root/profiles"
```

A zero target completes the mandatory automatic topology pass and starts no refinement pass. The
aggregate rows and `scaling_model_*.json` describe speedup, CPU work, and memory by width. Each
sample also writes `profiles/traces/<run_id>.csv`, whose interval effective-core and thread-count
series can reveal sequential startup/shutdown regions hidden by whole-step averages. This benchmark
can consume substantial time and disk space; remove `study_root` only after preserving any reports
or raw evidence you need. The same run writes `profiles/profile_report.html`, which combines the
speedup curves and interval traces in one interactive page.

## See also

- `dagrun --userguide` — the complete installed reference.
- `dagrun quickstart` — the same getting-started tour from the command line.
