## Install

```sh
cargo install tick-hub
```

Rust 1.85 or newer is required.

Add the crate as a dependency to embed the deterministic engine:

```toml
[dependencies]
tick-hub = "0.2"
```

## Rust API

The crate exports the validated model, serializers, cadence helpers, engine,
and injectable gate and file-age traits:

```rust
use tick_hub::{config_from_yaml, config_to_json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config_from_yaml("reminders: []\n")?;
    println!("{}", config_to_json(&config)?);
    Ok(())
}
```
