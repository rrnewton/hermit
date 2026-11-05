## Install

```sh
python3 -m pip install tick-hub
```

Python 3.10 or newer is required. Installation provides the `tick-hub` console
command, the typed `tick_hub` package, and JSON/YAML examples.

## Python API

The deterministic engine and its side-effecting boundaries are importable:

```python
from tick_hub import Emit, EmitKind, OpsState, Reminder, TickConfig

config = TickConfig(
    reminders=(Reminder("refresh", Emit(EmitKind.ACTION, "refresh", skill="cache")),)
)
state = OpsState.default()
```
