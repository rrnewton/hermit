"""Shared YAML-1.2-core scalar resolution, used by both the config and the ops-state loaders.

PyYAML defaults to YAML 1.1 implicit typing, which resolves UNQUOTED scalars differently from a
YAML-1.2 core schema: 1.1 turns ``no``/``yes``/``on``/``off`` into booleans (the "Norway problem"),
``0755`` into octal, ``1_000`` into a number, and ``2024-01-01`` into a date. Both tick-hub YAML
surfaces instead resolve plain scalars with YAML-1.2 core-schema rules; quoted and block scalars
always stay strings.

``core_load`` pins ``yaml.load``'s ``Any`` to ``object`` at the boundary; callers re-validate every
field's type, so no ``Any`` leaks past this module.
"""

from __future__ import annotations

import math
import re
from collections.abc import Hashable
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    # PyYAML is declared at runtime but imported lazily so `--help`, `--version`, and JSON-only
    # paths still provide useful behavior if an installation is damaged. These type-only imports
    # cost nothing at runtime.
    import yaml
    from yaml.nodes import ScalarNode

__all__ = ["core_load", "core_dump", "resolve_core_scalar"]

# Raised (as ModuleNotFoundError) by core_load / core_dump when PyYAML is absent; callers convert it
# to their own typed error so the CLI prints it cleanly instead of dumping a traceback.
_MISSING_YAML_MSG = (
    "reading or writing YAML requires PyYAML, but that package is not installed. "
    "Repair this installation with: python3 -m pip install 'pyyaml>=6'."
)

_RE_INT_DEC = re.compile(r"^[-+]?(0|[1-9][0-9]*)$")
_RE_INT_OCT = re.compile(r"^[-+]?0o[0-7]+$")
_RE_INT_HEX = re.compile(r"^[-+]?0x[0-9a-fA-F]+$")
_RE_INT_BIN = re.compile(r"^[-+]?0b[01]+$")
_RE_FLOAT = re.compile(
    r"^[-+]?(\.[0-9]+|[0-9]+\.[0-9]*)([eE][-+]?[0-9]+)?$" r"|^[-+]?[0-9]+[eE][-+]?[0-9]+$"
)
_NULL_TOKENS = frozenset({"", "~", "null", "Null", "NULL"})
_TRUE_TOKENS = frozenset({"true", "True", "TRUE"})
_FALSE_TOKENS = frozenset({"false", "False", "FALSE"})
_NONFINITE_TOKENS = frozenset(
    {
        ".inf", ".Inf", ".INF", "+.inf", "+.Inf", "+.INF", "-.inf", "-.Inf", "-.INF",
        ".nan", ".NaN", ".NAN",
    }
)

_CORE_TAG = "tag:tick-hub,2026:core-scalar"


def resolve_core_scalar(value: str) -> object:
    """Resolve one PLAIN YAML scalar to the type a YAML-1.2 core schema (serde_norway) would.

    Returns ``None`` / ``bool`` / ``int`` / ``float`` / ``str``. Leading-zero decimals, underscore
    grouping, sexagesimal, and timestamps stay STRINGS; ``0o``/``0x``/``0b`` and dotted/exponent
    floats parse; a float overflowing to infinity stays a string. Explicit ``.inf``/``.nan``
    tokens resolve as non-string markers and are rejected by :func:`core_load`."""
    if value in _NULL_TOKENS:
        return None
    if value in _TRUE_TOKENS:
        return True
    if value in _FALSE_TOKENS:
        return False
    if _RE_INT_DEC.match(value):
        return int(value, 10)
    if _RE_INT_OCT.match(value):
        return int(value, 0)
    if _RE_INT_HEX.match(value):
        return int(value, 0)
    if _RE_INT_BIN.match(value):
        return int(value, 0)
    if value in _NONFINITE_TOKENS:
        return None
    if _RE_FLOAT.match(value):
        parsed = float(value)
        return parsed if math.isfinite(parsed) else value
    return value


def _construct_core_scalar(loader: yaml.SafeLoader, node: ScalarNode) -> object:
    raw = loader.construct_scalar(node)
    if raw in _NONFINITE_TOKENS:
        import yaml

        raise yaml.constructor.ConstructorError(
            None,
            None,
            "non-finite YAML numbers are not supported",
            node.start_mark,
        )
    return resolve_core_scalar(raw)


# Caches for the lazily-built loader/dumper subclasses; PyYAML is imported and the subclasses
# defined only on first use (see _core_loader / _core_dumper), keeping the dependency optional.
_CORE_LOADER: type[yaml.SafeLoader] | None = None
_CORE_DUMPER: type[yaml.SafeDumper] | None = None


def _core_loader() -> type[yaml.SafeLoader]:
    """Build (once, lazily) the ``SafeLoader`` that resolves PLAIN scalars via core-schema rules."""
    global _CORE_LOADER
    if _CORE_LOADER is not None:
        return _CORE_LOADER
    import yaml

    class _CoreLoader(yaml.SafeLoader):
        def construct_mapping(
            self, node: yaml.MappingNode, deep: bool = False
        ) -> dict[Hashable, object]:
            result: dict[Hashable, object] = {}
            for key_node, value_node in node.value:
                key = self.construct_object(key_node, deep=deep)
                if not isinstance(key, str):
                    raise yaml.constructor.ConstructorError(
                        None,
                        None,
                        "mapping keys must be strings",
                        key_node.start_mark,
                    )
                if key == "<<":
                    raise yaml.constructor.ConstructorError(
                        None,
                        None,
                        "YAML merge keys are not supported",
                        key_node.start_mark,
                    )
                if key in result:
                    raise yaml.constructor.ConstructorError(
                        None,
                        None,
                        f"duplicate mapping key {key!r}",
                        key_node.start_mark,
                    )
                result[key] = self.construct_object(value_node, deep=deep)
            return result

    _CoreLoader.yaml_implicit_resolvers = {None: [(_CORE_TAG, re.compile(r".*", re.DOTALL))]}
    _CoreLoader.add_constructor(_CORE_TAG, _construct_core_scalar)
    _CORE_LOADER = _CoreLoader
    return _CoreLoader


def _represent_core_str(dumper: yaml.SafeDumper, data: str) -> ScalarNode:
    """Force-quote any string that :func:`resolve_core_scalar` would re-read as a NON-string."""
    style = "'" if not isinstance(resolve_core_scalar(data), str) else None
    return dumper.represent_scalar("tag:yaml.org,2002:str", data, style=style)


def _core_dumper() -> type[yaml.SafeDumper]:
    """Build (once, lazily) the ``SafeDumper`` whose representer round-trips through the loader."""
    global _CORE_DUMPER
    if _CORE_DUMPER is not None:
        return _CORE_DUMPER
    import yaml

    class _CoreDumper(yaml.SafeDumper):
        pass

    _CoreDumper.add_representer(str, _represent_core_str)
    _CORE_DUMPER = _CoreDumper
    return _CoreDumper


def core_load(text: str) -> object:
    """Parse YAML with core-schema plain-scalar resolution; result pinned to ``object``.

    Raises :class:`ModuleNotFoundError` (with an actionable install hint) if PyYAML is not
    installed; callers convert it to their own typed error.
    """
    try:
        import yaml
    except ModuleNotFoundError as exc:
        raise ModuleNotFoundError(_MISSING_YAML_MSG) from exc
    raw: object = yaml.load(text, Loader=_core_loader())
    return raw


def core_dump(obj: object) -> str:
    """Dump ``obj`` to a YAML document that round-trips back through :func:`core_load`.

    Raises :class:`ModuleNotFoundError` (with an actionable install hint) if PyYAML is not
    installed; callers convert it to their own typed error.
    """
    try:
        import yaml
    except ModuleNotFoundError as exc:
        raise ModuleNotFoundError(_MISSING_YAML_MSG) from exc
    return yaml.dump(
        obj,
        Dumper=_core_dumper(),
        sort_keys=False,
        allow_unicode=True,
        default_flow_style=False,
    )
