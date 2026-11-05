"""The typed runtime ops-state: per-host state read at the start of every tick.

This is the generic analog of a project's per-host ``.ops-state.yaml``. It is small, strict, and
typed: :class:`OpsState.load` deserializes the YAML into a dataclass where unknown top-level keys,
missing required fields, and wrong types are HARD errors (:class:`StateError`). Parsed YAML is
pinned to ``object`` at the boundary and every field is re-validated.

Two fields the ENGINE itself understands:

* ``enabled`` — master switch; when false the tick emits its state summary + health checks but fires
  no reminders.
* ``tick_frequency_min`` — the DESIRED tick cadence; if it differs from the actually-running cadence
  the tick emits an ``actualize-tick-frequency`` ACTION so a caller can reschedule.

Everything else a caller needs to gate reminders on goes in the extensible, typed ``flags`` map
(``bool`` / ``int`` / ``str`` values only) — the generic replacement for a pile of bespoke runtime
toggles. A :class:`~tick_hub.model.Reminder` names the flags it needs in ``requires_flags``.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field

from tick_hub.emit import format_action, format_note
from tick_hub.yamlcore import _MISSING_YAML_MSG, core_dump, core_load

DEFAULT_TICK_FREQUENCY_MIN = 30
_I64_MIN = -(2**63)
_I64_MAX = 2**63 - 1

_ALLOWED_TOP = {"enabled", "tick_frequency_min", "label", "flags"}

#: Values a flag may hold. Kept deliberately scalar so gating stays simple and serializable.
FlagValue = bool | int | str


class StateError(ValueError):
    """Strict ops-state validation failure."""


def _reject_unknown(d: Mapping[str, object], allowed: set[str], where: str) -> None:
    # Keys beginning with "_" are treated as inline comments/annotations and ignored.
    unknown = {k for k in d if k not in allowed and not str(k).startswith("_")}
    if unknown:
        raise StateError(f"{where}: unknown key(s) {sorted(unknown)} (allowed: {sorted(allowed)})")


def flag_truthy(flags: Mapping[str, FlagValue], name: str) -> bool:
    """True iff ``name`` is present in ``flags`` and truthy (True / non-zero / non-empty)."""
    if name not in flags:
        return False
    value = flags[name]
    if isinstance(value, bool):
        return value
    if isinstance(value, int):
        return value != 0
    return value != ""


@dataclass(frozen=True)
class OpsState:
    """Per-host runtime state for one tick."""

    enabled: bool
    tick_frequency_min: int
    label: str | None = None
    flags: Mapping[str, FlagValue] = field(default_factory=dict)

    @classmethod
    def default(cls) -> "OpsState":
        """A sensible fallback used when no state file is supplied: enabled, default cadence."""
        return cls(enabled=True, tick_frequency_min=DEFAULT_TICK_FREQUENCY_MIN)

    @classmethod
    def from_obj(cls, d: object) -> "OpsState":
        """Validate a decoded mapping and construct an operations state."""
        if not isinstance(d, dict):
            raise StateError("ops-state file must contain a top-level mapping")
        obj: dict[str, object] = {str(k): v for k, v in d.items()}
        _reject_unknown(obj, _ALLOWED_TOP, "top-level")

        enabled = obj.get("enabled")
        if not isinstance(enabled, bool):
            raise StateError(
                f"enabled must be a boolean (got {type(enabled).__name__}: {enabled!r})"
            )

        tick = obj.get("tick_frequency_min", DEFAULT_TICK_FREQUENCY_MIN)
        if isinstance(tick, bool) or not isinstance(tick, int):
            raise StateError(f"tick_frequency_min must be an integer (got {tick!r})")
        if tick <= 0:
            raise StateError(f"tick_frequency_min must be positive (got {tick})")
        if tick > _I64_MAX:
            raise StateError(
                f"tick_frequency_min is outside the signed 64-bit range (got {tick})"
            )

        label_raw = obj.get("label")
        if label_raw is not None and not isinstance(label_raw, str):
            raise StateError(
                f"label must be a string or null (got {type(label_raw).__name__}: {label_raw!r})"
            )
        label: str | None = (
            label_raw.strip() if isinstance(label_raw, str) and label_raw.strip() else None
        )

        if "flags" in obj and obj["flags"] is None:
            raise StateError("flags must be a mapping (got NoneType)")
        flags = _parse_flags(obj.get("flags"))
        return cls(enabled=enabled, tick_frequency_min=tick, label=label, flags=flags)

    @classmethod
    def from_yaml(cls, text: str) -> "OpsState":
        """Parse an ops-state YAML document (core-schema plain scalars) into an :class:`OpsState`.

        If the declared PyYAML dependency is missing, this raises :class:`StateError` with an
        actionable repair hint."""
        try:
            import yaml
        except ModuleNotFoundError as exc:
            raise StateError(_MISSING_YAML_MSG) from exc
        try:
            raw = core_load(text)
        except yaml.YAMLError as exc:
            raise StateError(f"invalid YAML: {exc}") from exc
        return cls.from_obj(raw)

    @classmethod
    def load(cls, path: str) -> "OpsState":
        """Read and parse an operations-state YAML file."""
        with open(path, encoding="utf-8") as fh:
            return cls.from_yaml(fh.read())

    def to_obj(self) -> dict[str, object]:
        """The plain, JSON/YAML-serializable mapping for this state (flags sorted for determinism)."""
        for key in self.flags:
            if not isinstance(key, str):
                raise StateError("flags: mapping keys must be strings")
        candidate: dict[str, object] = {
            "enabled": self.enabled,
            "tick_frequency_min": self.tick_frequency_min,
            "label": self.label,
            "flags": {k: self.flags[k] for k in sorted(self.flags)},
        }
        validated = type(self).from_obj(candidate)
        return {
            "enabled": validated.enabled,
            "tick_frequency_min": validated.tick_frequency_min,
            "label": validated.label,
            "flags": {k: validated.flags[k] for k in sorted(validated.flags)},
        }

    def to_yaml(self) -> str:
        """Serialize to a YAML document that round-trips back through :meth:`from_yaml`.

        If the declared PyYAML dependency is missing, this raises :class:`StateError` with an
        actionable repair hint."""
        try:
            return core_dump(self.to_obj())
        except ModuleNotFoundError as exc:
            raise StateError(_MISSING_YAML_MSG) from exc


def _parse_flags(value: object) -> dict[str, FlagValue]:
    if value is None:
        return {}
    if not isinstance(value, dict):
        raise StateError(f"flags must be a mapping (got {type(value).__name__})")
    out: dict[str, FlagValue] = {}
    for key, raw in value.items():
        name = str(key)
        if name == "<<":
            raise StateError("flags: mapping key '<<' is reserved and not supported")
        # bool must be checked before int (bool is a subclass of int).
        if isinstance(raw, bool):
            out[name] = raw
        elif isinstance(raw, int):
            if raw < _I64_MIN or raw > _I64_MAX:
                raise StateError(f"flags.{name} integer is outside the signed 64-bit range: {raw}")
            out[name] = raw
        elif isinstance(raw, str):
            out[name] = raw
        else:
            raise StateError(
                f"flags.{name} must be a boolean, integer, or string "
                f"(got {type(raw).__name__}: {raw!r})"
            )
    return out


def _flags_summary(flags: Mapping[str, FlagValue]) -> str:
    if not flags:
        return ""
    rendered = ",".join(
        f"{k}={('true' if v else 'false') if isinstance(v, bool) else v}"
        for k, v in sorted(flags.items())
    )
    return f" flags={rendered}"


def state_lines(state: OpsState, current_tick_min: int | None) -> list[str]:
    """The state-machine's own NOTE/ACTION lines for this tick.

    Always leads with a one-line state summary NOTE. Emits an ``actualize-tick-frequency`` ACTION
    when the desired cadence differs from the running one, and a NOTE when the hub is disabled.
    """
    label_part = f" label={state.label}" if state.label else ""
    lines = [
        format_note(
            f"ops-state enabled={'true' if state.enabled else 'false'} "
            f"tick_frequency_min={state.tick_frequency_min}"
            f"{label_part}{_flags_summary(state.flags)}"
        )
    ]
    if current_tick_min is not None and current_tick_min != state.tick_frequency_min:
        lines.append(
            format_action(
                "actualize-tick-frequency",
                {"desired": str(state.tick_frequency_min), "current": str(current_tick_min)},
                title=(
                    f"reschedule the tick to {state.tick_frequency_min} min so it matches "
                    f"the intended cadence (currently {current_tick_min} min)"
                ),
            )
        )
    if not state.enabled:
        lines.append(
            format_note(
                "ops-state disabled — no reminders will fire this tick; "
                "set enabled: true to activate"
            )
        )
    return lines
