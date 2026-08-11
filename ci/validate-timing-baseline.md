# Portable validation timing baseline

`validate-timing-baseline.json` is a reviewed merge-gate input, not an
observability report. `ci/run-node.sh` checks every selected portable node after
the runner exits, and complete local `portable-only`/`full` validation invokes
the same verifier from typed `StepOutcome` data.

The gate refuses missing or duplicate rows, a non-exact SHA, failed rows,
timeouts, OOM kills, and elapsed wall time above the recorded node p90 times the
recorded regression factor. The policy also refuses any baseline with fewer
than five exact-SHA cold runs, incomplete/failed samples, or a node p90 above
540 seconds. The 13 `e2e.manifest_*` jobs are audited separately by the hosted
E2E matrix; this baseline covers every node in the profiled `run-node.sh`
fan-out, whose membership is versioned in `ci/portable-shards.json`.

## Baseline changes

A baseline change must be its own explicit, adversarially reviewed change:

1. Collect at least five fresh GitHub-hosted ephemeral-VM runs at distinct exact
   SHAs through the production portable workflow. Workflow caches and build
   artifacts remain part of that production path; no worktree or profile store
   may survive between samples.
2. Require every profiled fan-out node in every run, with zero node failures,
   wall/CPU timeouts, and OOM kills.
3. Record the run IDs, exact SHAs, raw per-node wall samples, and recompute the
   nearest-rank p90. Every p90 must remain at or below 540 seconds.
4. Increment `baseline_version` and explain why accepting the measured slowdown
   is correct. Never raise a DAG timeout or this baseline merely to green a PR.
5. Run both brackets:

   ```sh
   python3 ci/validate-timing-gate.py --self-test
   python3 ci/validate-timing-gate.py --replay-incident
   ```

The second command must continue to reject `92aaed5d0`'s 730-second
`test.strict_compat` observation. If it does not, the baseline change has
disabled the prevention mechanism.
