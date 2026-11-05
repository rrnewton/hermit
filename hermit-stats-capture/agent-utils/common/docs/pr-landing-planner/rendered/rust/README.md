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

## Install

```sh
cargo install pr-landing-planner
```

Rust 1.85 or newer is required.

Add the crate as a dependency when planning belongs inside an application:

```toml
[dependencies]
pr-landing-planner = "0.1"
```

## Rust API

The crate exposes `FakeHost` and the `VcsHost` trait alongside collection,
classification, graph, planning, priority, and rendering modules. The pure core
accepts already-collected models, making deterministic fixture tests and custom
host adapters straightforward.

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
