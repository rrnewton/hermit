## Installation and library use

```sh
cargo install tick-hub
```

Rust 1.85 or newer is required. For library use, declare the dependency and
import the crate as `tick_hub`:

```toml
[dependencies]
tick-hub = "0.2"
```

```rust
use tick_hub::{config_from_yaml, config_to_json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config_from_yaml("reminders: []\n")?;
    println!("{}", config_to_json(&config)?);
    Ok(())
}
```

Public modules expose the validated model, cadence state, line emitters,
filesystem and subprocess probes, and the deterministic `run_tick` engine.
Implement `GateRunner` and `FileAgeProbe` to inject controlled boundaries.
