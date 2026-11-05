# tick-hub

`tick-hub` lets one cron job, timer, or coordinator heartbeat evaluate many
recurring responsibilities, each on its own cadence. It emits stable `HEALTH`,
`ACTION`, `NOTE`, and `ERROR` lines for people or automation to consume.

Reminders, shell gates, freshness checks, and emitted actions are caller-owned
JSON or YAML configuration. The engine is deterministic when given an explicit
clock value, and it persists cadence state only when asked to flush.

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

## Quick start

Save this as `ops.yaml`:

```yaml
reminders:
  - name: refresh_cache
    cadence_secs: 3600
    emit:
      kind: action
      skill: refresh-cache
      title: refresh the shared cache
health_checks:
  - name: newest_snapshot
    glob: /srv/snapshots/*.tar.zst
    threshold_secs: 86400
```

Run a dry tick or persist its updated cadence state:

```sh
tick-hub tick --config ops.yaml --no-header
tick-hub tick --config ops.yaml --flush --no-header
```

Explore the complete interface with:

```sh
tick-hub quickstart
tick-hub --help
tick-hub --userguide
```

## Output contract

- `HEALTH:` reports `ok`, `stale`, or `missing` freshness.
- `ACTION:` identifies one handler plus fields and a title.
- `NOTE:` carries informational state.
- `ERROR:` reports a gate or operational failure.
- `NO_RESULT:` reports that a gate could not determine its condition or that a
  declared dependent is unevaluable. It is neither a pass nor a failure.

The prefix is the record type. Consumers should parse that prefix and treat the
remaining fields as line-oriented data.

## License

MIT
