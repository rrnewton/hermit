# Version-controlled compatibility scorecard

Machine-readable, per-cell, and **idempotent**: re-witnessing the same green at
the same tip produces a byte-identical CSV, so `validate` can run twice and leave
**zero git diff**.

## Layout

```
compat-scorecard/
  scorecard.py                       write | lint | aggregate
  pre-commit-scorecard-lint.sh       blocks a committed capability DECREASE
  machines/
    .gitignore                       keeps run-metadata.json OUT of git
    <shortname>-<cpu-model>/
      scorecard.csv                  VERSION CONTROLLED, capability only
      run-metadata.json              GITIGNORED, everything run-varying
```

The machine key is `<shortname>-<cpu-model>`; only the first label of the
hostname is used, so an internal FQDN never reaches a tracked path.

## Crux 1 — idempotent / no churn

The scorecard records **observed capability only**. Anything that differs between
two runs of the same code — `run_id`, `duration_ms`, `binary_sha256`,
`hermit_sha`, `source_tree_dirty`, timestamps — is run metadata and goes to the
gitignored `run-metadata.json`. Rows are sorted and columns fixed, so input file
order cannot perturb the output either.

A file that churns on every run cannot be reviewed, and a diff that is always
noise is a diff nobody reads.

## Crux 2 — monotonicity as a pre-commit lint

Capability may increase freely. A **decrease is blocked at commit time** unless
the change carries **both**:

1. a strong reason — *fake-green found*, or *deliberate temporary regression* —
   in `decrease-reason.txt` beside the scorecard, so it is reviewed in the same
   diff that lowers the number; and
2. a named **P0** that will not close until the capability is restored.

Deleting a row counts as a decrease, so a regression cannot be hidden by removing
the cell.

Install: `ln -s ../../compat-scorecard/pre-commit-scorecard-lint.sh .git/hooks/pre-commit`
(or call it from an existing hook).

## Row schema

`bucket, test, mode, backend, strict, detlog_stack, detlog_heap, chaos, determinism, parity`

- **mode** — `run` | `replay` (the harness spells the run-mode `verify`).
- **tier booleans** — ordered weaker → stricter, so gaining `--detlog-heap` is
  strictly more capability.
- **determinism** — the `--verify` double-run passed. Only `verify` runs a second
  execution, so any other mode is **blank = UNMEASURED**, never `0`.
- **parity** — this backend's log matched the ptrace reference. The harness JSONL
  carries no cross-backend comparison, so this is currently **blank =
  UNMEASURED** rather than invented; a separate producer fills it.

## Usage

```
scorecard.py write     --results ignored/e2e/*/*/results.jsonl
scorecard.py aggregate --csv compat-scorecard/machines/<key>/scorecard.csv
scorecard.py lint      --old <committed.csv> --new <staged.csv> [--reason FILE]
```

`aggregate` renders ptrace-first backend columns × bucket rows, a `TOTAL` row and
a `TOTAL-OF-TOTALS` cell.
