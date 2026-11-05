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
