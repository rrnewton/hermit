# Affected-Test Selection

Two small, dependency-free tools decide **which** CI nodes a change needs and
rank nodes by **cost vs. value**, so a limited-footprint change runs a subset of
the suite instead of all of it, and provably CI-irrelevant changes skip CI
entirely.

| Tool | Question it answers |
| --- | --- |
| `ci/select-tests.rs` | Given a change, which portable-DAG nodes can it affect? |
| `ci/power-to-weight.rs` | Which nodes cost the most per unit of value delivered? |

Both read `ci/dag/portable.json` as the single source of truth for the node
universe, node commands, and build dependencies. `select-tests.rs` adds one
thing the DAG does not encode: the **source-path → node** relation, held in
`ci/test-footprints.json`.

## The selection model

A change is a set of changed files. Each file is classified:

- **force_full** — a file whose blast radius cannot be reasoned about locally
  (build config, toolchain, the CI harness itself, `validate.sh`). Any such file
  ⇒ run the entire suite.
- **footprint** — a file matching a `paths` glob contributes that entry's
  `nodes` (unioned across all matching entries).
- **ci_irrelevant** — a provably inert file (docs, notes, images). Contributes
  nothing.
- **unknown** — a file matching no rule. Treated like force_full: ⇒ entire suite.

The outcome is one of three decisions:

- **skip** — *every* changed file is ci_irrelevant. Run nothing.
- **selective** — files map to a subset. Run that subset, closed over the DAG's
  build `deps`, plus a small always-on preflight (`lint.rustfmt`,
  `check.backend_abstraction`, `check.portability_paths`).
- **full** — a force_full or unknown file appeared, or no baseline is trusted.

### Fail-safe by construction

The only decision that runs *fewer* nodes than a change might need is **skip**,
and skip requires positive proof that *every* file is inert. Every doubt —
unknown path, a footprint node missing from the current DAG, files not proven
inert — resolves to **full**. Therefore a mismapped footprint can only waste
time, never hide a regression. The one place a mistake could wrongly skip is the
`ci_irrelevant` list, so it is kept deliberately tight (docs, notes, images,
non-workflow `.github/**`; the three real workflow files are force_full).

## Two delta contexts

Selection is a delta **against a green baseline** — it runs the tests the delta
can affect and trusts the baseline for everything else. The delta is defined
differently in the two places CI runs:

| Context | Delta | Invocation |
| --- | --- | --- |
| **GitHub PR** | the PR's own contribution vs the target branch | `ci/select-tests.rs --base origin/main` (uses `origin/main...HEAD`, the merge-base) |
| **Local `validate.sh`** | dirty working copy + commits since the last known-green commit | `ci/select-tests.rs --since-green --baseline <sha>` |

`--since-green` computes `committed-since-baseline ∪ staged ∪ unstaged ∪
untracked`. The baseline SHA comes from `--baseline` or the
`HERMIT_LAST_GREEN_SHA` environment variable.

### Soundness prerequisite: a real green baseline

Selection is only sound if the baseline it trusts is *actually* green. Two
consequences:

1. **No trustworthy baseline ⇒ full suite.** In `--since-green` mode with no
   `--baseline` and no `HERMIT_LAST_GREEN_SHA`, the tool refuses to trust an
   unknown baseline and returns **full**. It never skips on an unproven baseline.
2. **The baseline SHA is supplied by the validate-run-ledger.** This tool is
   storage-agnostic: it does not decide what "green" means or remember past
   runs. The [`validate-run-ledger`](../../CLAUDE.md) records, per slot, the last
   commit whose validate run was green; a `validate.sh` wrapper reads that SHA
   and passes it in. See "Integration contract" below.

> **Known blocker.** A robustly-green baseline does not yet exist on developer
> hosts: full `validate.sh` cannot exit 0 on a devserver (host-sensitive detcore
> tests — futex-absolute-timeout, RDRAND/RDSEED — plus the DynamoRIO
> cold-checkout failure). Until that is resolved the *local* baseline is
> untrustworthy and `--since-green` correctly falls back to full. The GitHub
> context is unaffected: `origin/main` gated by required checks is the green
> baseline there.

## Usage

```bash
# GitHub PR context: what does this PR's diff require?
ci/select-tests.rs --base origin/main

# Local context: what changed since my last green validate run?
ci/select-tests.rs --since-green --baseline "$(cat .last-green-sha)"

# Explicit file list (from anywhere):
git show --name-only <sha> | ci/select-tests.rs --files -

# Emit $GITHUB_OUTPUT (decision / skip / full / node_count / nodes):
ci/select-tests.rs --base origin/main --format github

# Self-test (no git, no network):
ci/select-tests.rs --self-test
```

`--format json` prints a machine-readable object; `--format github` appends the
gating variables to `$GITHUB_OUTPUT` so a workflow can conditionally run its
matrix.

## Power-to-weight

`ci/power-to-weight.rs` joins each node's **cost** (`hint.est_duration_s`, which
are **hand-estimated**, not measured — see `ci/dag/README`) with a **value**
proxy: how often `select-tests.rs` would actually select the node across a sample
of recent commits. Low selection-rate ÷ high cost = a candidate to move off the
per-commit critical path onto a nightly lane.

```bash
ci/power-to-weight.rs --sample 200            # human table
ci/power-to-weight.rs --format csv > pw.csv    # artifact
```

Two honesty caveats are built into the output:

- Selection rate is measured on **past** commits; it predicts future value only
  if the change mix stays similar. A recent window dominated by CI/infra changes
  (which force full) inflates every node's rate.
- A low-power node is **ranked, not condemned**. Moving it to nightly trades
  per-commit latency for slower regression detection on a rarely-touched
  subsystem. Confirm with the owning area, and prefer real durations from the
  validate-run-ledger over the estimates once available.

## Integration contract (validate-run-ledger)

The selector consumes exactly one fact from the ledger: **the last-known-green
commit SHA for the current slot**. The interface is intentionally minimal:

- The ledger (or a `validate.sh` wrapper over it) exports
  `HERMIT_LAST_GREEN_SHA=<40-hex>` or passes `--baseline <40-hex>`.
- If that SHA is absent or empty, the selector returns **full** (fail-safe).
- The selector does not write to the ledger and does not define "green"; the
  ledger owns both.

## What is not here yet

- **Live workflow wiring.** These tools compute a decision; no workflow yet gates
  its matrix on it. Wiring `ci-portable.yml` to skip/subset based on
  `--format github` is a follow-up to coordinate with the CI-DAG owner, and
  depends on the green-baseline blocker above for the local path.
- **Measured durations.** power-to-weight uses hand estimates until the ledger
  provides observed per-node durations.
