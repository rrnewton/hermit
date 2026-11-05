# tick-hub user guide

One scheduled invocation can carry many independently cadenced reminders and
freshness checks. `tick-hub` decides what is due, evaluates optional gates, and
prints a deterministic line protocol. It does not schedule itself or dispatch
the actions it emits.

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

## Configuration

Configuration is strict JSON or YAML with two top-level collections:

```yaml
description: routine maintenance heartbeat
reminders:
  - name: refresh_cache
    cadence_secs: 3600
    requires_flags: [maintenance_enabled]
    gate:
      cmd: test -f /srv/cache/stale
      when: success
      capture: false
      parallel: false
    emit:
      kind: action
      skill: refresh-cache
      fields: {priority: normal}
      title: refresh the shared cache
  - name: heartbeat_note
    cadence_secs: 0
    emit:
      kind: note
      title: heartbeat evaluated
health_checks:
  - name: newest_snapshot
    glob: /srv/snapshots/*.tar.zst
    threshold_secs: 86400
    detail: newest data snapshot
```

`cadence_secs: 0` means every tick. A positive cadence means the reminder is
due when at least that many seconds have elapsed since its last evaluation.
Names must be non-empty and unique within reminders or health checks. Reminder
names are fired-state keys and therefore cannot contain whitespace or `=`, or
start with the reserved `__tick_hub_internal__.` namespace.
Unknown fields, wrong types, negative cadences or thresholds, reserved `<<`
mapping keys, and duplicate mapping keys are errors.

`depends_on` names other reminders in the same configuration. Unknown,
duplicate, self-referential, or cyclic dependency edges are errors. Every due
gate still runs. If a dependency exits 75 (`NO_RESULT`) and the dependent gate
is otherwise quiet, the dependent emits its own `NO_RESULT` as unevaluable and
does not consume cadence. A dependent that finds a real problem still emits it
verbatim; dependency metadata never suppresses a finding. Empty output or an
intentional zero selection does not imply `NO_RESULT`: only exit 75 carries
that meaning.

An action emit requires a non-empty `skill`; its `fields` values are strings.
A note emit uses `title` as its text.

## One tick

```sh
tick-hub tick --config ops.yaml
```

This is a dry run: it evaluates the tick and prints records without writing the
advanced fired-state. Add `--flush` to persist it:

```sh
tick-hub tick --config ops.yaml --flush
```

The default fired-state path is `./.tick-hub/state`. Override it with
`--fired-state FILE` or `TICK_HUB_STATE`. `--now EPOCH` pins the non-negative
clock value for reproducible tests. `--current-tick-min N` requires a positive
cadence. Both numeric flags are bounded to signed 64-bit values. `--no-header`
suppresses the explanatory standard-error banner so standard output contains
only protocol lines.

The same atomic file also carries `__tick_hub_internal__.*` retry diagnostics.
These keys are managed by tick-hub, are never cadence timestamps, and are
pruned when their reminder is removed. In particular, unresolved template
rendering stores a consecutive count and first-failure epoch. As with cadence,
these updates become durable only with `--flush`; dry runs report what would
happen without mutating the file.

## Output records

Every standard-output line begins with a stable record type:

```text
HEALTH: newest_snapshot ok age_secs=120 threshold_secs=86400 detail="newest data snapshot"
NOTE: ops-state enabled=true tick_frequency_min=30 flags=maintenance_enabled=true
ACTION: refresh-cache priority=normal title="refresh the shared cache"
NOTE: emitted 1 instruction(s) this tick
```

- `HEALTH:` reports the newest file matching a glob as `ok`, `stale`, or
  `missing`.
- `ACTION:` names one caller-defined handler and carries ordered `key=value`
  fields plus a quoted title.
- `NOTE:` is informational and requires no dispatch.
- `ERROR:` means a gate or its configured emission could not complete; that
  reminder is retried later.
- `NO_RESULT:` means a gate could not determine its condition, or a quiet
  dependent is unevaluable because such a gate is its declared dependency. It
  is neither a pass nor a failure and does not consume cadence.

If a fired reminder cannot render because a `{placeholder}` is unresolved,
tick-hub emits an `ERROR:` and a counted `NO-SIGNAL` action and retries the
reminder on the next tick without consuming cadence. The third consecutive
render failure, and every consecutive failure after it, adds another
`NO-SIGNAL` action carrying the count and first-failure epoch; it does not
replace or suppress the original records. Any later evaluated outcome that is
not a render failure clears that reminder's internal failure streak.

Consumers should dispatch only the record types they understand and retain
unknown fields for forward compatibility.

## Shell gates and captured values

A due reminder may run one shell gate. `when` selects the fire condition:

| Value | Fire when |
|---|---|
| `success` | The command exits zero. |
| `failure` | The command exits nonzero. |
| `nonempty` | Standard output contains non-whitespace text. |
| `always` | The command completed, regardless of status. |

With `capture: true`, lines in the form `key=value` become action fields.
Configured field values and titles may fill `{key}` placeholders from both
configured and captured fields:

```yaml
- name: queue_depth
  gate:
    cmd: printf 'count=12\n'
    when: always
    capture: true
  emit:
    kind: action
    skill: drain-queue
    title: drain {count} queued items
```

Captured values override same-named configured fields. A failed-to-start or
timed-out gate emits `ERROR:` and does not consume the reminder's cadence.
Gate commands execute through `bash` and should be treated as trusted config.

Gates run one at a time unless at least two due gates explicitly set
`parallel: true`. The production runner starts those permitted gates together
before the serial pass and still reports every result in configuration order.
Leave the field false unless the command can safely overlap every other due
gate: commands that update shared state, acquire the same lock, or depend on
another gate's side effects must remain serial.

## Per-host state and flags

An optional state file supplies a master switch, desired scheduler cadence,
label, and scalar flags:

```yaml
enabled: true
tick_frequency_min: 15
label: worker-a
flags:
  maintenance_enabled: true
  region: west
```

```sh
tick-hub tick --config ops.yaml --state host.yaml --current-tick-min 30
tick-hub state --state host.yaml --current-tick-min 30
```

All names in `requires_flags` must be present and truthy. A suppressed reminder
does not consume its cadence. When `enabled` is false, health and state records
still print but reminders do not run. If `--current-tick-min` differs from the
desired value, the state contributes an `actualize-tick-frequency` action.

## Inspect and convert

```sh
tick-hub list --config ops.yaml
tick-hub json --config ops.yaml > ops.json
tick-hub yaml --config ops.json > ops.yaml
```

Both encodings map to the same validated model. The conversion commands are
useful for validation and canonicalization.

## Command summary

| Command | Purpose |
|---|---|
| `tick` | Evaluate one heartbeat; write cadence state only with `--flush`. |
| `state` | Validate and render a per-host state file. |
| `list` | List configured reminders and health checks. |
| `json` / `yaml` | Validate and convert configuration. |
| `quickstart` | Print a runnable introduction. |

All commands support `--help`; the top level supports `--version` and
`--userguide`.

## Operational guidance

Run exactly one scheduler entry at a cadence no slower than the shortest
important reminder. Use `--flush` in that real scheduler entry, but omit it in
manual previews. Keep the state file on persistent storage and do not run two
flushing invocations against the same file concurrently. Keep action handlers
idempotent: an interrupted process may emit an action before its state write.

## License

MIT
