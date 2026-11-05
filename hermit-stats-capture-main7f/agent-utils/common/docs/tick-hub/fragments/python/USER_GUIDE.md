## Installation and library use

```sh
python3 -m pip install tick-hub
```

Python 3.10 or newer is required. Installation provides the console command,
the typed `tick_hub` package, examples, and this guide as package data.

```python
from tick_hub import Emit, EmitKind, OpsState, Reminder, TickConfig, run_tick
from tick_hub.probes import GlobFileAgeProbe, SubprocessGateRunner

config = TickConfig(
    reminders=(Reminder("refresh", Emit(EmitKind.ACTION, "refresh", skill="cache")),)
)
result = run_tick(
    config,
    OpsState.default(),
    now=0,
    fired={},
    gate_runner=SubprocessGateRunner(),
    age_probe=GlobFileAgeProbe(),
)
```

`GateRunner` and `FileAgeProbe` are protocols, so applications can inject
controlled implementations while keeping cadence and output evaluation pure.
