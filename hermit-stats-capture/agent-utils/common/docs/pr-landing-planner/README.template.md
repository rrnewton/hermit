# pr-landing-planner

`pr-landing-planner` is an advisory pull-request landing planner. It combines
real merge-conflict detection, dependency ordering, CI state, freshness, holds,
mechanism overlap, and caller-provided validation evidence into deterministic
per-PR actions and parallel-safe groups.

The planner reports recommendations. It never merges, rebases, labels, refires,
or otherwise mutates a pull request.

Landing evidence is fail-closed: required reviews bind the exact fetched head,
local validation records name the exact tested head and base, the consuming
workspace supplies explicit hard/soft-green authorization for every clean
validation record, and dependency cycles
or non-landable predecessors cannot enter a batch.

{{DISTRIBUTION}}

## Quick start

Generate and plan a deterministic, network-free fixture:

```sh
pr-landing-planner quickstart --emit-demo > demo.yaml
pr-landing-planner plan --fixture demo.yaml
pr-landing-planner graph --fixture demo.yaml --format json
```

Plan a live repository using read-only metadata and a local clone:

```sh
pr-landing-planner plan \
  --repo OWNER/NAME \
  --base main \
  --git-dir /path/to/clone
```

Live collection requires `gh` and `git` on `PATH`. Fixture planning requires no
network access.

Discover the complete interface with:

```sh
pr-landing-planner quickstart
pr-landing-planner --help
pr-landing-planner --userguide
```

## Outputs

- `plan` emits recommended actions and parallel-safe groups.
- `graph` emits conflict and ordering edges.
- `clusters` groups PRs that should move as one stack.
- `status` summarizes CI and label health.

Human and canonical JSON formats are available throughout. `plan` also offers
a stable line-oriented `actions` format for automation.

## License

MIT
