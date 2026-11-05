## Installation and library use

```sh
cargo install pr-landing-planner
```

Rust 1.85 or newer is required. Live collection also requires `gh` and `git` on
`PATH`. For library use, declare the dependency and import it as
`pr_landing_planner`:

```toml
[dependencies]
pr-landing-planner = "0.1"
```

The crate exports `FakeHost`, `GitHubHost`, the `VcsHost` trait, collection
options, CI classifiers, graph builders, planning functions, typed models, and
deterministic renderers. The host boundary is a trait so applications can feed
saved snapshots or another repository service into the pure planning core.
