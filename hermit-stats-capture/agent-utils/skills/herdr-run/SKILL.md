---
name: herdr-run
description: Run a cooperatively allowlisted command in a terminal pane belonging to a separate terminal server, preserving output, status, readiness evidence, serialization, and an operational audit trail. Use when a command needs to run outside whatever constrains this process - most often because a sandbox blocks a legitimate git, gh, or dependency-fetch operation.
---

# herdr-run

Use this command only for operations admitted by the project policy. Its allowlist is a cooperative
safety rail, not a same-user containment boundary; do not widen policy without reading the trust
model.

The installed CLI is the source of truth:

- `herdr-run --help`
- `herdr-run quickstart` for the one-screen introduction
- `herdr-run status` to see the configuration, policy, and session in effect; it changes nothing
- `herdr-run config` to inspect the fully resolved policy
- `herdr-run init` to write an annotated `.herdr-run.yaml` -- but note that `allow` inside it is a
  human-only knob: do not add programs to it on your own initiative
- a `herdr-run: WARNING: ... leaves out built-in deny entries` line means the project's file is
  older than the tool and those entries are not enforced; tell the owner, whose fix is
  `herdr-run init --upgrade` in the directory holding the file
- `herdr-run check '<command>'`
- `herdr-run --agent NAME run --dry-run '<command>'` to validate and render without touching a pane
- `herdr-run net-doctor`
- `herdr-run userguide`

Dry-runs and refusals still append operational evidence to `.herdr-run/audit.jsonl`. Successful
runs keep `command`, byte-exact `stdout`/`stderr`, `exit_code`, and `meta.json` under
`.herdr-run/runs/<run_id>/`; `--json` reports that path. Completed run spools are pruned on a later
write after the configured retention window (four days by default), while active runs are retained.
