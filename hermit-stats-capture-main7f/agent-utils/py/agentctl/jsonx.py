"""Narrowing helpers for JSON that arrives as ``object``.

This package is type-checked with mypy ``strict`` plus ``disallow_any_explicit``, so decoded JSON
cannot be annotated ``Any`` and then indexed freely. Every field crossing the Herdr API boundary is
narrowed here with an explicit type check, which also means a Herdr protocol change surfaces as a
clear "field X is not a Y" error instead of an ``AttributeError`` three frames later.
"""

from __future__ import annotations

__all__ = ["as_mapping", "as_sequence", "get_str", "get_int", "opt_str", "dig"]


def as_mapping(value: object, what: str) -> dict[str, object]:
    """Require ``value`` to be a JSON object."""
    if not isinstance(value, dict):
        raise TypeError(f"{what}: expected an object, got {type(value).__name__}")
    out: dict[str, object] = {}
    for key, item in value.items():
        if not isinstance(key, str):
            raise TypeError(f"{what}: expected string keys, got {type(key).__name__}")
        out[key] = item
    return out


def as_sequence(value: object, what: str) -> list[object]:
    """Require ``value`` to be a JSON array."""
    if not isinstance(value, list):
        raise TypeError(f"{what}: expected an array, got {type(value).__name__}")
    return list(value)


def get_str(mapping: dict[str, object], key: str, what: str) -> str:
    """Require ``mapping[key]`` to be present and a string."""
    value = mapping.get(key)
    if not isinstance(value, str):
        raise TypeError(f"{what}: field {key!r} is not a string")
    return value


def get_int(mapping: dict[str, object], key: str, what: str) -> int:
    """Require ``mapping[key]`` to be present and an integer.

    ``bool`` is excluded explicitly: it is an ``int`` subclass in Python, and silently accepting
    ``true`` as ``1`` would hide a genuine protocol mismatch.
    """
    value = mapping.get(key)
    if isinstance(value, bool) or not isinstance(value, int):
        raise TypeError(f"{what}: field {key!r} is not an integer")
    return value


def opt_str(mapping: dict[str, object], key: str) -> str | None:
    """Return an absent/null/string field, rejecting every other protocol shape."""
    value = mapping.get(key)
    if value is None or isinstance(value, str):
        return value
    raise TypeError(f"field {key!r} is not a string")


def dig(value: object, path: tuple[str, ...], what: str) -> object:
    """Walk a chain of object keys, naming the exact step that was missing when it fails."""
    current = value
    for index, key in enumerate(path):
        mapping = as_mapping(current, f"{what} at {'.'.join(path[:index]) or '<root>'}")
        if key not in mapping:
            raise TypeError(f"{what}: missing field {'.'.join(path[: index + 1])}")
        current = mapping[key]
    return current
