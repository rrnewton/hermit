"""tick-hub: one scheduled tick funnels many cadenced reminders into machine-readable output.

Describe your recurring responsibilities as a :class:`TickConfig` — :class:`Reminder` values (each
with a cadence, an optional shell :class:`Gate`, and an :class:`Emit` template) plus
:class:`HealthCheck` freshness probes — and call :func:`run_tick` once per heartbeat with the
per-host :class:`OpsState` and an explicit ``now``. You get a stable, line-oriented report:

    HEALTH: <check> <ok|stale|missing> age_secs=<N|NA> threshold_secs=<N> detail="..."
    ACTION: <skill> [key=value ...] title="..."
    NOTE:   <free text>
    ERROR:  <text>

The engine is pure and deterministic given ``now``; the two side-effecting boundaries (running a
gate command, measuring a file age) are the pluggable :class:`GateRunner` / :class:`FileAgeProbe`
protocols, with production implementations in :mod:`tick_hub.probes` and fakes in the tests.

    from tick_hub import TickConfig, Reminder, Emit, EmitKind, OpsState, run_tick
    from tick_hub.probes import SubprocessGateRunner, GlobFileAgeProbe
    cfg = TickConfig(reminders=(Reminder("sync", Emit(EmitKind.ACTION, "sync now", skill="sync")),))
    result = run_tick(cfg, OpsState.default(), now=0, fired={},
                      gate_runner=SubprocessGateRunner(), age_probe=GlobFileAgeProbe())
    for line in result.lines:
        print(line)
"""

from __future__ import annotations

from tick_hub.cadence import (
    due_reminders,
    is_due,
    load_fired_state,
    persist_fired_state,
)
from tick_hub.emit import (
    HEALTH_STATUS_MISSING,
    HEALTH_STATUS_OK,
    HEALTH_STATUS_STALE,
    format_action,
    format_error,
    format_health,
    format_note,
)
from tick_hub.engine import (
    TickResult,
    evaluate_health,
    parse_kv_lines,
    render_emit,
    run_tick,
)
from tick_hub.io import (
    TickConfigError,
    config_from_json,
    config_from_yaml,
    config_to_json,
    config_to_yaml,
)
from tick_hub.model import (
    EVERY_TICK,
    Emit,
    EmitKind,
    Gate,
    GateWhen,
    HealthCheck,
    Reminder,
    TickConfig,
)
from tick_hub.probes import GlobFileAgeProbe, SubprocessGateRunner, wall_clock_now
from tick_hub.protocols import FileAgeProbe, GateResult, GateRunner
from tick_hub.state import (
    DEFAULT_TICK_FREQUENCY_MIN,
    OpsState,
    StateError,
    flag_truthy,
    state_lines,
)

__version__: str = "0.2.0"

__all__ = [
    "__version__",
    # config model
    "TickConfig",
    "Reminder",
    "Emit",
    "EmitKind",
    "Gate",
    "GateWhen",
    "HealthCheck",
    "EVERY_TICK",
    # config serialization
    "config_from_json",
    "config_from_yaml",
    "config_to_json",
    "config_to_yaml",
    "TickConfigError",
    # ops-state
    "OpsState",
    "StateError",
    "state_lines",
    "flag_truthy",
    "DEFAULT_TICK_FREQUENCY_MIN",
    # cadence / fired-state
    "is_due",
    "due_reminders",
    "load_fired_state",
    "persist_fired_state",
    # engine
    "run_tick",
    "TickResult",
    "evaluate_health",
    "render_emit",
    "parse_kv_lines",
    # emitters
    "format_action",
    "format_note",
    "format_error",
    "format_health",
    "HEALTH_STATUS_OK",
    "HEALTH_STATUS_STALE",
    "HEALTH_STATUS_MISSING",
    # pluggable boundaries
    "GateRunner",
    "GateResult",
    "FileAgeProbe",
    "SubprocessGateRunner",
    "GlobFileAgeProbe",
    "wall_clock_now",
]
