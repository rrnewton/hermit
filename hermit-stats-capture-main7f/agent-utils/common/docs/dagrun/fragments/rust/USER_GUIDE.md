## Installation and library use

```sh
cargo install dagrun
```

Rust 1.85 or newer is required. The crate installs both binaries. For library
use, declare the dependency; the crate and its import name are the same word:

```toml
[dependencies]
dagrun = "0.15"
```

```rust
use dagrun::{dag_from_yaml, to_ascii};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dag = dag_from_yaml("steps: []")?;
    println!("{}", to_ascii(&dag, None));
    Ok(())
}
```

Public modules cover containment, CPU reservations, resource sizing, profile
summaries, deterministic planning, scheduling, serialization, and
visualization. The `run_dag` function is the explicit uncontained scheduler;
`run_dag_boxed` applies a caller-supplied cgroup manager when present. The
console command establishes and verifies the outer systemd scope. Enforced
containment requires Linux with cgroup v2 and a delegated systemd user scope.
`run_dag(..., combined_limit)` and `run_dag_boxed(..., combined_limit)` treat
that argument as a compatibility combined active-step and per-step-width limit.
Use `run_dag_limited(..., max_steps, max_cpus, ...)` or a boxed limited variant
to choose the two values independently. Without an externally established outer
quota, these helpers do not cap aggregate CPU bandwidth or serialize overlapping
widths. `cap_config_max_cpus` applies the same per-step cap to runner-controlled
commands without starting a run; it leaves
self-managed fixed widths unchanged so the run helpers can refuse an
over-budget command truthfully.
The low-level `allocate_widths(...)` helper returns
`Result<HashMap<_, _>, InfeasibleAllocationError>`; 0.15 makes an over-budget
self-managed fixed command an explicit error rather than an executable-looking
width map.
