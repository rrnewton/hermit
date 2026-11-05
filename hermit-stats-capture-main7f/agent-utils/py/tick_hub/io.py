"""Canonical JSON and YAML (de)serialization for a :class:`~tick_hub.model.TickConfig`.

This is the on-disk / interchange form the CLI loads via ``--config FILE``. The schema mirrors the
dataclasses field-for-field. Parsing is STRICT and fails loudly on a malformed document
(:class:`TickConfigError`), never silently defaulting a wrong-typed field.

YAML (:func:`config_from_yaml` / :func:`config_to_yaml`) is ISOMORPHIC to the JSON schema: the
parsed YAML object is funneled through the SAME strict narrowing as JSON, so both syntaxes build an
identical model. YAML additionally allows comments and multi-line block scalars (handy for
``description`` fields). Plain (unquoted) YAML scalars are resolved with YAML-1.2 core-schema rules,
so ``no``/``yes``/``on``/``off`` stay strings rather than becoming booleans.

``json.loads`` / ``yaml.load`` yield ``Any``; both are pinned to ``object`` at the parse boundary
and every field's type is re-validated here, so no ``Any`` leaks into the model.
"""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from typing import TYPE_CHECKING, NoReturn

if TYPE_CHECKING:
    # PyYAML is declared but imported lazily, keeping JSON paths and startup diagnostics usable if
    # an installation is damaged.
    import yaml

from tick_hub.cadence import INTERNAL_STATE_PREFIX
from tick_hub.model import (
    Emit,
    EmitKind,
    Gate,
    GateWhen,
    HealthCheck,
    Reminder,
    TickConfig,
)
from tick_hub.yamlcore import _MISSING_YAML_MSG, core_dump, core_load

__all__ = [
    "config_from_json",
    "config_from_yaml",
    "config_to_json",
    "config_to_yaml",
    "TickConfigError",
]

# Bound arbitrary-precision integers to the format's documented signed 64-bit range.
_I64_MIN = -(2**63)
_I64_MAX = 2**63 - 1


class TickConfigError(ValueError):
    """Raised when a tick-hub config document is malformed."""


def _check_i64(n: int, where: str) -> int:
    if not (_I64_MIN <= n <= _I64_MAX):
        raise TickConfigError(f"{where}: integer {n} does not fit a signed 64-bit range")
    return n


# --- typed narrowing helpers (json.loads yields Any; narrow explicitly, no Any leaks) ---


def _as_obj(value: object, where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise TickConfigError(f"{where}: expected an object, got {type(value).__name__}")
    out: dict[str, object] = {}
    for key, val in value.items():
        if not isinstance(key, str):
            raise TickConfigError(f"{where}: object keys must be strings")
        if key == "<<":
            raise TickConfigError(f"{where}: mapping key '<<' is reserved and not supported")
        out[key] = val
    return out


def _reject_unknown(m: Mapping[str, object], allowed: frozenset[str], where: str) -> None:
    unknown = sorted(set(m) - allowed)
    if unknown:
        raise TickConfigError(f"{where}: unknown field(s): {', '.join(unknown)}")


def _require_nonempty(value: str, field: str, where: str) -> str:
    if not value.strip():
        raise TickConfigError(f"{where}: field '{field}' must be non-empty")
    return value


def _require_reminder_name(value: str, where: str) -> str:
    value = _require_nonempty(value, "name", where)
    if "=" in value or any(char.isspace() for char in value):
        raise TickConfigError(
            f"{where}: field 'name' must not contain whitespace or '=' "
            "(it is a fired-state key)"
        )
    if value.startswith(INTERNAL_STATE_PREFIX):
        raise TickConfigError(
            f"{where}: field 'name' must not start with reserved internal-state prefix "
            f"{INTERNAL_STATE_PREFIX!r}"
        )
    return value


def _req_str(m: Mapping[str, object], key: str, where: str) -> str:
    val = m.get(key)
    if not isinstance(val, str):
        raise TickConfigError(f"{where}: field '{key}' must be a string")
    return val


def _opt_str(m: Mapping[str, object], key: str, default: str, where: str) -> str:
    val = m.get(key, default)
    if not isinstance(val, str):
        raise TickConfigError(f"{where}: field '{key}' must be a string")
    return val


def _opt_int(m: Mapping[str, object], key: str, default: int, where: str) -> int:
    val = m.get(key, default)
    if isinstance(val, bool) or not isinstance(val, int):
        raise TickConfigError(f"{where}: field '{key}' must be an integer")
    return _check_i64(val, f"{where}.{key}")


def _opt_nonnegative_int(m: Mapping[str, object], key: str, default: int, where: str) -> int:
    value = _opt_int(m, key, default, where)
    if value < 0:
        raise TickConfigError(f"{where}: field '{key}' must be non-negative")
    return value


def _opt_positive_int_or_none(
    m: Mapping[str, object], key: str, where: str
) -> int | None:
    if key not in m:
        return None
    value = _opt_int(m, key, 0, where)
    if value <= 0:
        raise TickConfigError(f"{where}: field '{key}' must be positive")
    return value


def _opt_bool(m: Mapping[str, object], key: str, default: bool, where: str) -> bool:
    val = m.get(key, default)
    if not isinstance(val, bool):
        raise TickConfigError(f"{where}: field '{key}' must be a boolean")
    return val


def _opt_str_list(m: Mapping[str, object], key: str, where: str) -> tuple[str, ...]:
    if key not in m:
        return ()
    val = m[key]
    if not isinstance(val, list):
        raise TickConfigError(f"{where}: field '{key}' must be a list of strings")
    out: list[str] = []
    for item in val:
        if not isinstance(item, str):
            raise TickConfigError(f"{where}: field '{key}' must contain only strings")
        out.append(item)
    return tuple(out)


def _opt_str_str_map(m: Mapping[str, object], key: str, where: str) -> dict[str, str]:
    if key not in m:
        return {}
    val = m[key]
    obj = _as_obj(val, f"{where}.{key}")
    out: dict[str, str] = {}
    for name, text in obj.items():
        if not isinstance(text, str):
            raise TickConfigError(f"{where}.{key}.{name}: must be a string")
        out[name] = text
    return out


def _gate_from(value: object, where: str) -> Gate | None:
    if value is None:
        return None
    obj = _as_obj(value, where)
    _reject_unknown(
        obj, frozenset({"cmd", "when", "capture", "timeout_secs", "parallel"}), where
    )
    when_name = _opt_str(obj, "when", GateWhen.SUCCESS.value, where)
    try:
        when = GateWhen(when_name)
    except ValueError as exc:
        raise TickConfigError(
            f"{where}.when: unknown value {when_name!r} "
            f"(allowed: {[w.value for w in GateWhen]})"
        ) from exc
    return Gate(
        cmd=_req_str(obj, "cmd", where),
        when=when,
        capture=_opt_bool(obj, "capture", False, where),
        timeout_secs=_opt_positive_int_or_none(obj, "timeout_secs", where),
        parallel=_opt_bool(obj, "parallel", False, where),
    )


def _emit_from(value: object, where: str) -> Emit:
    obj = _as_obj(value, where)
    _reject_unknown(obj, frozenset({"kind", "title", "skill", "fields"}), where)
    kind_name = _opt_str(obj, "kind", EmitKind.ACTION.value, where)
    try:
        kind = EmitKind(kind_name)
    except ValueError as exc:
        raise TickConfigError(
            f"{where}.kind: unknown value {kind_name!r} "
            f"(allowed: {[k.value for k in EmitKind]})"
        ) from exc
    emit = Emit(
        kind=kind,
        title=_opt_str(obj, "title", "", where),
        skill=_opt_str(obj, "skill", "", where),
        fields=_opt_str_str_map(obj, "fields", where),
    )
    if emit.kind is EmitKind.ACTION and not emit.skill.strip():
        raise TickConfigError(f"{where}: an ACTION emit requires a non-empty 'skill'")
    return emit


def _reminder_from(value: object, where: str) -> Reminder:
    obj = _as_obj(value, where)
    _reject_unknown(
        obj,
        frozenset(
            {
                "name",
                "emit",
                "cadence_secs",
                "cadence_offset_secs",
                "cadence_window_secs",
                "requires_flags",
                "depends_on",
                "gate",
            }
        ),
        where,
    )
    emit_raw = obj.get("emit")
    if emit_raw is None:
        raise TickConfigError(f"{where}: field 'emit' is required")
    depends_on = _opt_str_list(obj, "depends_on", where)
    for index, dependency in enumerate(depends_on):
        _require_reminder_name(dependency, f"{where}.depends_on[{index}]")
    cadence_secs = _opt_nonnegative_int(obj, "cadence_secs", 0, where)
    cadence_offset_secs: int | None = None
    if obj.get("cadence_offset_secs") is not None:
        # Refuse a phase that cannot mean what it says, rather than silently
        # normalising it: an offset on an every-tick reminder has no window to sit
        # in, and an offset at or beyond the cadence aliases onto a different
        # window than the author wrote. Both are configuration mistakes that would
        # otherwise show up as a reminder quietly running on the wrong tick.
        cadence_offset_secs = _opt_nonnegative_int(obj, "cadence_offset_secs", 0, where)
        if cadence_secs <= 0:
            raise TickConfigError(
                f"{where}: 'cadence_offset_secs' requires a positive 'cadence_secs'; "
                "an every-tick reminder is always due and cannot be phased"
            )
        if cadence_offset_secs >= cadence_secs:
            raise TickConfigError(
                f"{where}: 'cadence_offset_secs' ({cadence_offset_secs}) must be less than "
                f"'cadence_secs' ({cadence_secs}); a larger offset names the same instants "
                "as offset % cadence and hides which tick the author meant"
            )
    cadence_window_secs: int | None = None
    if obj.get("cadence_window_secs") is not None:
        cadence_window_secs = _opt_nonnegative_int(obj, "cadence_window_secs", 0, where)
        if cadence_offset_secs is None:
            raise TickConfigError(
                f"{where}: 'cadence_window_secs' requires 'cadence_offset_secs'; "
                "a window only means anything relative to a phase"
            )
        if not 0 < cadence_window_secs <= cadence_secs:
            raise TickConfigError(
                f"{where}: 'cadence_window_secs' ({cadence_window_secs}) must be greater "
                f"than 0 and at most 'cadence_secs' ({cadence_secs})"
            )
    return Reminder(
        name=_require_reminder_name(_req_str(obj, "name", where), where),
        emit=_emit_from(emit_raw, f"{where}.emit"),
        cadence_secs=cadence_secs,
        cadence_offset_secs=cadence_offset_secs,
        cadence_window_secs=cadence_window_secs,
        requires_flags=_opt_str_list(obj, "requires_flags", where),
        depends_on=depends_on,
        gate=_gate_from(obj.get("gate"), f"{where}.gate"),
    )


def _validate_dependencies(reminders: Sequence[Reminder]) -> None:
    by_name = {reminder.name: reminder for reminder in reminders}
    for reminder in reminders:
        if len(set(reminder.depends_on)) != len(reminder.depends_on):
            raise TickConfigError(
                f"<root>: reminder {reminder.name!r} has duplicate depends_on entries"
            )
        for dependency in reminder.depends_on:
            if dependency == reminder.name:
                raise TickConfigError(
                    f"<root>: reminder {reminder.name!r} cannot depend on itself"
                )
            if dependency not in by_name:
                raise TickConfigError(
                    f"<root>: reminder {reminder.name!r} depends on unknown reminder "
                    f"{dependency!r}"
                )

    visiting: set[str] = set()
    visited: set[str] = set()

    def visit(name: str, path: tuple[str, ...]) -> None:
        if name in visited:
            return
        if name in visiting:
            cycle = " -> ".join((*path, name))
            raise TickConfigError(f"<root>: reminder dependency cycle: {cycle}")
        visiting.add(name)
        for dependency in by_name[name].depends_on:
            visit(dependency, (*path, name))
        visiting.remove(name)
        visited.add(name)

    for reminder in reminders:
        visit(reminder.name, ())


def _health_from(value: object, where: str) -> HealthCheck:
    obj = _as_obj(value, where)
    _reject_unknown(obj, frozenset({"name", "glob", "threshold_secs", "detail"}), where)
    return HealthCheck(
        name=_require_nonempty(_req_str(obj, "name", where), "name", where),
        glob=_require_nonempty(_req_str(obj, "glob", where), "glob", where),
        threshold_secs=_opt_nonnegative_int(obj, "threshold_secs", 0, where),
        detail=_opt_str(obj, "detail", "", where),
    )


def _reject_json_constant(token: str) -> NoReturn:
    """Reject JSON's non-standard ``Infinity`` / ``-Infinity`` / ``NaN`` literals.

    Python's ``json`` accepts these by default, but they are outside this format's strict JSON
    contract."""
    raise TickConfigError(f"invalid JSON: non-finite float literal {token!r} is not allowed")


def config_from_json(text: str) -> TickConfig:
    """Parse a tick-hub config JSON document into a :class:`TickConfig`."""
    try:
        def unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
            result: dict[str, object] = {}
            for key, value in pairs:
                if key in result:
                    raise TickConfigError(f"invalid JSON: duplicate object key {key!r}")
                result[key] = value
            return result

        raw: object = json.loads(
            text,
            parse_constant=_reject_json_constant,
            object_pairs_hook=unique_object,
        )
    except json.JSONDecodeError as exc:
        raise TickConfigError(f"invalid JSON: {exc}") from exc
    return _config_from_obj(raw)


def config_from_yaml(text: str) -> TickConfig:
    """Parse a tick-hub config YAML document into a :class:`TickConfig`.

    YAML is ISOMORPHIC to the JSON schema: the parsed object funnels through the same typed
    narrowing (:func:`_config_from_obj`), and plain scalars resolve with YAML-1.2 core-schema rules
    (:mod:`tick_hub.yamlcore`).

    If the declared PyYAML dependency is missing, this raises :class:`TickConfigError` with an
    actionable repair hint (JSON remains available)."""
    try:
        import yaml
    except ModuleNotFoundError as exc:
        raise TickConfigError(_MISSING_YAML_MSG) from exc
    try:
        raw: object = core_load(text)
    except yaml.YAMLError as exc:
        raise TickConfigError(f"invalid YAML: {exc}") from exc
    return _config_from_obj(raw)


def _config_from_obj(raw: object) -> TickConfig:
    doc = _as_obj(raw, "<root>")
    _reject_unknown(doc, frozenset({"reminders", "health_checks", "description"}), "<root>")
    reminders: list[Reminder] = []
    if "reminders" in doc:
        reminders_raw = doc["reminders"]
        if not isinstance(reminders_raw, list):
            raise TickConfigError("<root>: 'reminders' must be a list")
        for i, entry in enumerate(reminders_raw):
            reminders.append(_reminder_from(entry, f"reminders[{i}]"))
    reminder_names = [reminder.name for reminder in reminders]
    if len(set(reminder_names)) != len(reminder_names):
        raise TickConfigError("<root>: reminder names must be unique")
    _validate_dependencies(reminders)
    health: list[HealthCheck] = []
    if "health_checks" in doc:
        health_raw = doc["health_checks"]
        if not isinstance(health_raw, list):
            raise TickConfigError("<root>: 'health_checks' must be a list")
        for i, entry in enumerate(health_raw):
            health.append(_health_from(entry, f"health_checks[{i}]"))
    health_names = [check.name for check in health]
    if len(set(health_names)) != len(health_names):
        raise TickConfigError("<root>: health check names must be unique")
    return TickConfig(
        reminders=tuple(reminders),
        health_checks=tuple(health),
        description=_opt_str(doc, "description", "", "<root>"),
    )


# --- serialization (canonical, deterministic) ---


def _gate_to_obj(gate: Gate | None) -> object:
    if gate is None:
        return None
    obj: dict[str, object] = {
        "cmd": gate.cmd,
        "when": gate.when.value,
        "capture": gate.capture,
    }
    if gate.timeout_secs is not None:
        obj["timeout_secs"] = gate.timeout_secs
    if gate.parallel:
        obj["parallel"] = True
    return obj


def _emit_to_obj(emit: Emit) -> dict[str, object]:
    if "<<" in emit.fields:
        raise TickConfigError("emit.fields: mapping key '<<' is reserved and not supported")
    return {
        "kind": emit.kind.value,
        "title": emit.title,
        "skill": emit.skill,
        "fields": dict(sorted(emit.fields.items())),
    }


def _reminder_to_obj(rem: Reminder) -> dict[str, object]:
    obj: dict[str, object] = {
        "name": rem.name,
        "cadence_secs": rem.cadence_secs,
        **(
            {"cadence_offset_secs": rem.cadence_offset_secs}
            if rem.cadence_offset_secs is not None
            else {}
        ),
        **(
            {"cadence_window_secs": rem.cadence_window_secs}
            if rem.cadence_window_secs is not None
            else {}
        ),
        "requires_flags": list(rem.requires_flags),
        "gate": _gate_to_obj(rem.gate),
        "emit": _emit_to_obj(rem.emit),
    }
    if rem.depends_on:
        obj["depends_on"] = list(rem.depends_on)
    return obj


def _health_to_obj(hc: HealthCheck) -> dict[str, object]:
    return {
        "name": hc.name,
        "glob": hc.glob,
        "threshold_secs": hc.threshold_secs,
        "detail": hc.detail,
    }


def _config_to_obj(cfg: TickConfig) -> dict[str, object]:
    obj: dict[str, object] = {
        "description": cfg.description,
        "health_checks": [_health_to_obj(h) for h in cfg.health_checks],
        "reminders": [_reminder_to_obj(r) for r in cfg.reminders],
    }
    # Public dataclasses are directly constructible. Validate them through the same strict
    # narrowing path before serializing so every emitted document is guaranteed loadable.
    _config_from_obj(obj)
    return obj


def config_to_json(cfg: TickConfig) -> str:
    """Serialize a :class:`TickConfig` to canonical, deterministic JSON (2-space indent)."""
    return json.dumps(_config_to_obj(cfg), indent=2, ensure_ascii=False)


def config_to_yaml(cfg: TickConfig) -> str:
    """Serialize a :class:`TickConfig` to a YAML document that round-trips through
    :func:`config_from_yaml`.

    If the declared PyYAML dependency is missing, this raises :class:`TickConfigError` with an
    actionable repair hint (JSON remains available)."""
    try:
        return core_dump(_config_to_obj(cfg))
    except ModuleNotFoundError as exc:
        raise TickConfigError(_MISSING_YAML_MSG) from exc
