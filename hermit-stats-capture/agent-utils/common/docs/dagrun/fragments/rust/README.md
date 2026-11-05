## Install

```sh
cargo install dagrun
```

Rust 1.85 or newer is required.

The crate installs the `dagrun` and `cpuset-alloc` binaries. Add it as a
library dependency when the model or planning engine belongs inside an
application:

```toml
[dependencies]
dagrun = "0.15"
```

## Rust API

The crate exports the model, strict serializers, planner, scheduler, and
visualization helpers:

```rust
use dagrun::{dag_from_yaml, to_ascii};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dag = dag_from_yaml("steps: []")?;
    println!("{}", to_ascii(&dag, None));
    Ok(())
}
```

Use `dag_from_path(path)` for a JSON/YAML file that may contain namespaced
`include` fragments. The context-free `dag_from_json` and `dag_from_yaml`
functions deliberately refuse `include` because they have no safe base directory.

`run_dag(..., combined_limit)` keeps a compatibility combined setting: that
number bounds active steps and caps each runner-controlled step's width. Call
`run_dag_limited(..., max_steps, max_cpus, ...)` (or the corresponding boxed
limited helper) when those limits differ. `cap_config_max_cpus` exposes the same
per-step capping policy for runner-controlled commands. It deliberately
leaves a self-managed fixed width unchanged; the run helpers then reject it if
it exceeds `max_cpus`. Library helpers do not establish an outer scope, so
`max_cpus` is a whole-run bandwidth cap only when the caller supplies equivalent
outer containment; it does not serialize steps whose declared widths sum above
that value.

For Rust harnesses, cargo-nextest supplies libtest's `--exact TEST` arguments,
so the process snapshot can bind each child to its test. Ordinary `cargo test`
runs several tests inside one shared binary; its process tree alone does not
identify the live test and remains explicitly unattributed.

A step declares structured test evidence in `result_manifests`. The runner then exports a
scheduler-owned `DAGRUN_TEST_COUNTS_PATH` to that step and requires a current result there.
Steps without that typed declaration do not receive the path and do not owe this evidence.
There is no process-wide switch that can override the graph's declaration.
A controlled test framework writes exactly one current JSON object to that path:

```json
{"schema":2,"executed_tests":2,"filtered_tests":5,"results":[{"id":"suite$passes","result":"pass","attempts":1},{"id":"suite$recovers","result":"pass","attempts":2}]}
```

A required result-producing step fails immediately when the file is missing, malformed, or does
not match its declared schema. Retained schema-1 count-only files remain readable through the
compatibility parser, but cannot satisfy a schema-2 producer. Printing a line that looks like
a libtest summary cannot create receipt evidence in this mode.

Schema 2 remains the default: `TestResult::new`, `TestResults::current`, and
`StructuredTestResultsManifest::current` retain their existing output. Frameworks
with complete native attempt records may explicitly select
`StructuredTestResultsManifest::classified` and write schema 3 with
`TestResult::with_attempt_results` and `TestResults::classified().write_classified(...)`.
Each attempt records its one-based index, outcome (`passed`, `failed`, `cpu_timeout`,
`wall_timeout`, `cancelled`, `infrastructure_error`, or `no_result`), and detail.
Every nonpass requires a nonempty trimmed detail; a pass has null detail. The
complete consecutive history must agree with its terminal pass/fail result.
Executed counts still count named tests, not attempts.

Schema 4 (`StructuredTestResultsManifest::diagnostic`,
`TestResults::diagnostic().write_diagnostic_typed(...)`) is schema 3 plus one row kind.
A row whose `result` is `diagnostic_fail` is a failed test whose producer declares it cannot
decide the step; the producer's reason is required in `diagnostic_reason`, which is null on
every other row. Build one with `TestResult::diagnostic_failure`. Only a measured failure can
be diagnostic: the terminal attempt must be `failed`, `cpu_timeout` or `wall_timeout`, never
`cancelled`, `infrastructure_error` or `no_result`. The scheduler does not fail the step on
such a row, but it prints a `DIAGNOSTIC` count and the first ten failures with their causes
and reasons, records `diagnostic_test_failures` in the step's journal record, and adds one
end-of-run line naming each step with diagnostic failures and the total. The row stays in
`StepOutcome.test_results` with `passed: false` (`TestResult::is_diagnostic_failure`).
Everything else still fails the step: any other non-pass row, any row of schema 2 or 3, and a
producer that exits non-zero whatever its rows say. The scheduler trusts the producer to
decide which tests are diagnostic; declaring schema 4 on a step is what grants that trust.
Schema 2 and 3 writers refuse a diagnostic row rather than turn it into a plain failure, and a
schema 3 reader refuses the `diagnostic_fail` value.

Readers require the exact declared schema. Terminal-only rows cannot be promoted into
classified evidence, and schema-2 writers refuse to discard typed causes.
Duplicate JSON keys, missing causes, and contradictory histories refuse.
`StepOutcome.test_results_error` retains a required-result refusal separately
from a simultaneous process, CPU, wall, OOM or cancellation cause. A terminal
failed test cannot become a pass because its framework process exited zero.
This additive API adds `TestResult.attempt_results` and
`StepOutcome.test_results_error`, and schema 4 adds `TestResult.diagnostic`:
downstream exhaustive struct patterns need `..` or the new fields, and struct literals must
initialize the optional fields.

`resource_caps` apply within one runner process by default. To apply the same
capacities across independent runners, pass `run --resource-caps-path FILE`.
`DAGRUN_RESOURCE_CAPS_PATH=FILE` is the secondary route for launchers where a
flag is hard to add; the flag wins when both are present. A run using the file
prints its path before any node starts. With neither input, no shared file is
read and enforcement remains process-local.

`FILE` must already exist, be a regular file owned by the effective user, have
one hard link, and be readable and writable. A missing configured file is an
error; dagrun does not create an empty state and pretend global caps are active.
Create an empty file before starting any runners:

```sh
umask 077
printf '%s\n' '{"next_ticket":0,"requests":[]}' > /absolute/path/resource-caps.json
dagrun run --dag pipeline.yaml \
  --resource-caps-path /absolute/path/resource-caps.json
```

The current JSON schema is one object with these fields:

```json
{
  "next_ticket": 2,
  "requests": [
    {
      "pid": 1234,
      "starttime": 987654,
      "request": "1234-987654-1",
      "tag": "build.app",
      "ticket": 1,
      "state": "waiting",
      "resources": {
        "browser": {"demand": 1, "capacity": 2}
      }
    }
  ]
}
```

`next_ticket`, `pid`, `starttime`, and `ticket` are non-negative JSON integers,
except that `pid` and `starttime` must be positive. Request and tag values are
non-empty strings. State is `waiting` or `held`. `resources` is a non-empty
object whose names are non-empty strings; every demand and capacity is a
positive integer, demand must not exceed capacity, and live requests for one
resource must record the same capacity. Request values and ticket values are
unique, and each ticket is less than `next_ticket`.

The file currently has no schema version, boot identity, run identity, or
timestamp. The process start time protects against PID reuse during one boot,
but the file cannot distinguish a stale record from a later boot if the PID and
start time happen to match. The request string is derived from PID, process
start time, and ticket; it identifies a request, not a run. Those omissions,
the strict ticket ordering, and the choice of machine-wide capacities must be
resolved before this should be treated as a generally trustworthy host policy.

This is a repairable design, not one that needs replacement. A next schema
should carry a schema version, the host boot identity, the dagrun run identity,
and the times at which each request was written and granted. The reader should
refuse versions it does not understand, and records from a different boot
should not consume current capacity. The run identity and times should make it
possible to connect a ledger record to the run that produced it and to explain
how long it waited. The machine policy must also establish the capacities used
by every participating runner. Finally, ticket ordering should be revised so a
request that cannot yet fit does not leave otherwise usable capacity idle. The
current locking, atomic file replacement, strict parsing, and process start-time
checks remain useful parts of that repair.

The environment-only implementation that preceded the flag was invisible in
`--help` and startup output. It also treated an absent configured file as an
empty state. Both behaviors made it impossible to tell whether global caps were
actually in effect. The current interface exposes the opt-in and refuses the
missing file. A waiting step does not start its wall or CPU timer until every
requested capacity is granted, and child commands do not inherit the path
because they are already inside the outer step's grant. Malformed state and
conflicting capacities refuse before a node starts.
