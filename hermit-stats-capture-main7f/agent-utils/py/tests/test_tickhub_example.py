"""The shipped example config + state load, are isomorphic (JSON/YAML twins), and drive a tick."""

from __future__ import annotations

from pathlib import Path

from tick_hub.io import config_from_json, config_from_yaml, config_to_json
from tick_hub.engine import run_tick
from tick_hub.probes import GlobFileAgeProbe
from tick_hub.protocols import GateResult
from tick_hub.state import OpsState

_EXAMPLES = Path(__file__).resolve().parent.parent / "tick_hub" / "examples"


class _NoGate:
    """A gate runner that should never be called (the example tick below uses real echo gates via
    the engine's default runner only in the isomorphism-free path); here we inject canned output."""

    def __init__(self) -> None:
        self.calls: list[str] = []

    def run(self, cmd: str, *, timeout: int | None = None) -> GateResult:
        del timeout
        self.calls.append(cmd)
        # The example's capturing gate echoes count=42; its note gate exits 1 (stays quiet).
        if "count" in cmd:
            return GateResult(returncode=0, stdout="count=42\n", ok=True)
        return GateResult(returncode=1, stdout="", ok=True)


def test_example_yaml_and_json_are_isomorphic() -> None:
    yaml_cfg = config_from_yaml((_EXAMPLES / "tick-hub-ops.yaml").read_text(encoding="utf-8"))
    json_cfg = config_from_json((_EXAMPLES / "tick-hub-ops.json").read_text(encoding="utf-8"))
    assert config_to_json(yaml_cfg) == config_to_json(json_cfg)


def test_example_state_loads() -> None:
    state = OpsState.load(str(_EXAMPLES / "tick-hub-state.yaml"))
    assert state.enabled is True
    assert state.tick_frequency_min == 30
    assert state.flags.get("benchmark_enabled") is True


def test_example_tick_produces_expected_actions() -> None:
    cfg = config_from_yaml((_EXAMPLES / "tick-hub-ops.yaml").read_text(encoding="utf-8"))
    state = OpsState.load(str(_EXAMPLES / "tick-hub-state.yaml"))
    result = run_tick(
        cfg,
        state,
        now=0,
        fired={},
        gate_runner=_NoGate(),
        age_probe=GlobFileAgeProbe(),
        current_tick_min=30,
    )
    joined = "\n".join(result.lines)
    # health check glob matches nothing -> missing
    assert "HEALTH: db_backup missing" in joined
    # plain timed reminder (never fired) fires
    assert "ACTION: git-sync" in joined
    # gated + captured value interpolated
    assert 'ACTION: backlog-triage threshold=20 count=42 title="backlog has 42 ready items (threshold 20)"' in joined
    # flag-gated benchmark fires (state flag on)
    assert "ACTION: run-benchmark" in joined
    # note reminder's gate exits 1 (when=success) -> stays quiet
    assert "disk headroom is low" not in joined
