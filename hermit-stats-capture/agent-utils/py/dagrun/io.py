"""Strict JSON and YAML serialization for :class:`DagConfig`.

Both input formats narrow through the same schema and reject malformed or wrong-typed
fields with :class:`DagJsonError`. YAML additionally supports comments and block scalars.
"""

from __future__ import annotations

import json
import math
import re
from collections.abc import Mapping, Sequence
from copy import deepcopy
from pathlib import Path
from typing import TYPE_CHECKING, NoReturn

if TYPE_CHECKING:
    # PyYAML is a declared runtime dependency, but it is imported lazily so malformed installations
    # still retain working JSON paths and actionable `--help` / `--version` output. Under
    # `from __future__ import annotations` these type-only imports cost nothing at runtime.
    import yaml
    from yaml.nodes import ScalarNode

from dagrun.model import (
    DEFAULT_JOBS_FLAG,
    STRUCTURED_TEST_RESULTS_KIND,
    STRUCTURED_TEST_RESULTS_PATH_ENV,
    STRUCTURED_TEST_RESULTS_CURRENT_SCHEMA,
    STRUCTURED_TEST_RESULTS_CLASSIFIED_SCHEMA,
    STRUCTURED_TEST_RESULTS_DIAGNOSTIC_SCHEMA,
    CmdType,
    DagConfig,
    DagManifest,
    IntentionalSkipReason,
    ResourceHint,
    ResultManifest,
    Step,
    StepClass,
    StructuredTestResultsManifest,
    WriteDomainGuarantee,
    WriteDomainPolicy,
    graph_structure_violations,
    write_domain_violations,
    resolve_jobs_env,
    validate_cmdtype_config,
)

__all__ = [
    "MAX_DAG_FLATTENED_STEPS",
    "MAX_DAG_INCLUDE_DEPTH",
    "MAX_DAG_INCLUDE_INSTANCES",
    "dag_from_json",
    "dag_from_path",
    "dag_from_yaml",
    "dag_to_json",
    "dag_to_yaml",
    "DagJsonError",
]

_DEFAULT_MEM_CAP_FLOOR = 8 * 1024**3

# Path-composition safety bounds. The root document is depth zero and is not an include instance.
# These limits affect only dag_from_path; the context-free legacy parsers retain their existing
# graph-size behavior.
MAX_DAG_INCLUDE_DEPTH = 128
MAX_DAG_INCLUDE_INSTANCES = 4096
MAX_DAG_FLATTENED_STEPS = 100_000


class DagJsonError(ValueError):
    """Raised when a DAG JSON document is malformed."""


# Surfaced (as a DagJsonError, so the CLI prints it cleanly rather than dumping a traceback) when a
# YAML entry point is reached but the declared PyYAML dependency is not installed.
_MISSING_YAML_MSG = (
    "reading or writing YAML requires PyYAML, but that package is not installed. "
    "Repair this installation with: python3 -m pip install 'pyyaml>=6'. "
    "The JSON format remains available."
)


# Integer fields map to Rust `i64`, and the Rust build reads them via serde_json's `as_i64`, which
# rejects anything outside this range (including u64 values above i64::MAX). Python ints are
# arbitrary-precision, so bound them here too or a config the Python build accepts would be
# unreadable by the Rust build.
_I64_MIN = -(2**63)
_I64_MAX = 2**63 - 1


def _check_i64(n: int, where: str) -> int:
    if not (_I64_MIN <= n <= _I64_MAX):
        raise DagJsonError(f"{where}: integer {n} does not fit a signed 64-bit range")
    return n


# --- typed narrowing helpers (json.loads yields Any; narrow explicitly, no Any leaks) ---


def _as_obj(value: object, where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise DagJsonError(f"{where}: expected an object, got {_json_type_name(value)}")
    out: dict[str, object] = {}
    for key, val in value.items():
        out[str(key)] = val
    return out


def _json_type_name(value: object) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, str):
        return "str"
    if isinstance(value, list):
        return "list"
    if isinstance(value, dict):
        return "dict"
    return type(value).__name__


def _steps_list_error(doc: Mapping[str, object]) -> DagJsonError:
    keys = ", ".join(repr(key) for key in sorted(doc)) or "(none)"
    if "steps" not in doc:
        found = f"no 'steps' key (top-level keys: {keys})"
    else:
        found = f"'steps' with type {_json_type_name(doc['steps'])} (top-level keys: {keys})"
    return DagJsonError(
        "<root>: expected a dagrun DAG document with a top-level 'steps' list; "
        f"found {found}. This may be a different document type. Pass a dagrun DAG file, "
        "or run `dagrun quickstart` for the schema."
    )


def _req_str(m: Mapping[str, object], key: str, where: str) -> str:
    val = m.get(key)
    if not isinstance(val, str):
        raise DagJsonError(f"{where}: field '{key}' must be a string")
    return val


def _opt_str(m: Mapping[str, object], key: str, default: str) -> str:
    val = m.get(key, default)
    if not isinstance(val, str):
        raise DagJsonError(f"field '{key}' must be a string")
    return val


def _opt_int(m: Mapping[str, object], key: str, default: int) -> int:
    val = m.get(key, default)
    if isinstance(val, bool) or not isinstance(val, int):
        raise DagJsonError(f"field '{key}' must be an integer")
    return _check_i64(val, f"field '{key}'")


def _opt_str_or_none(m: Mapping[str, object], key: str) -> str | None:
    val = m.get(key)
    if val is None:
        return None
    if not isinstance(val, str):
        raise DagJsonError(f"field '{key}' must be a string or null")
    return val


def _jobs_env_field(m: Mapping[str, object], key: str, where: str) -> str | None:
    """Read and validate an optional jobs-env field with its document location attached."""
    raw = _opt_str_or_none(m, key)
    if raw is None:
        return None
    try:
        return resolve_jobs_env(raw, env={})
    except ValueError as exc:
        raise DagJsonError(f"{where}.{key}: {exc}") from exc


def _cmdtype_field(m: Mapping[str, object], where: str) -> CmdType:
    raw = _opt_str(m, "cmdtype", CmdType.UNKNOWN.value)
    try:
        return CmdType(raw)
    except ValueError as exc:
        valid = ", ".join(value.value for value in CmdType)
        raise DagJsonError(
            f"{where}.cmdtype: unknown value {raw!r}; valid values: {valid}"
        ) from exc


def _opt_int_or_none(m: Mapping[str, object], key: str) -> int | None:
    val = m.get(key)
    if val is None:
        return None
    if isinstance(val, bool) or not isinstance(val, int):
        raise DagJsonError(f"field '{key}' must be an integer or null")
    return _check_i64(val, f"field '{key}'")


def _opt_float(m: Mapping[str, object], key: str, default: float) -> float:
    val = m.get(key, default)
    if isinstance(val, bool) or not isinstance(val, (int, float)):
        raise DagJsonError(f"field '{key}' must be a number")
    result = float(val)
    # Reject NaN / +-inf: the Rust build's serde_json::Value cannot represent a non-finite number
    # (it rejects such input), and a non-finite value re-emits as invalid JSON (`Infinity`/`NaN`),
    # so both builds must refuse it to stay isomorphic and round-trippable.
    if not math.isfinite(result):
        raise DagJsonError(f"field '{key}' must be a finite number")
    return result


def _opt_float_or_none(m: Mapping[str, object], key: str) -> float | None:
    val = m.get(key)
    if val is None:
        return None
    if isinstance(val, bool) or not isinstance(val, (int, float)):
        raise DagJsonError(f"field '{key}' must be a number or null")
    result = float(val)
    if not math.isfinite(result):
        raise DagJsonError(f"field '{key}' must be a finite number or null")
    return result


def _opt_bool(m: Mapping[str, object], key: str, default: bool) -> bool:
    val = m.get(key, default)
    if not isinstance(val, bool):
        raise DagJsonError(f"field '{key}' must be a boolean")
    return val


def _opt_str_list(m: Mapping[str, object], key: str) -> list[str]:
    val = m.get(key)
    if val is None:
        return []
    if not isinstance(val, list):
        raise DagJsonError(f"field '{key}' must be a list of strings")
    out: list[str] = []
    for item in val:
        if not isinstance(item, str):
            raise DagJsonError(f"field '{key}' must contain only strings")
        out.append(item)
    return out


def _labels(m: Mapping[str, object], where: str) -> list[str]:
    labels = _opt_str_list(m, "labels")
    if any(not label.strip() for label in labels):
        raise DagJsonError(f"{where}.labels: labels must be non-empty")
    if len(labels) != len(set(labels)):
        raise DagJsonError(f"{where}.labels: duplicate labels are not allowed")
    return labels


def _present_str_list(m: Mapping[str, object], key: str) -> list[str] | None:
    """Parse a presence-sensitive string list.

    ``None`` means the key was omitted; an explicit empty list remains ``[]``.
    Write-domain fail-closed policy depends on that distinction.
    """

    if key not in m:
        return None
    val = m[key]
    if not isinstance(val, list):
        raise DagJsonError(f"field '{key}' must be a list of strings")
    out: list[str] = []
    for item in val:
        if not isinstance(item, str):
            raise DagJsonError(f"field '{key}' must contain only strings")
        out.append(item)
    return out


def _integration_test_binaries(m: Mapping[str, object]) -> list[str] | None:
    values = _present_str_list(m, "integration_test_binaries")
    if values is None:
        return None
    if any(not value.strip() for value in values):
        raise DagJsonError(
            "field 'integration_test_binaries' must not contain empty names"
        )
    duplicates = sorted({value for value in values if values.count(value) > 1})
    if duplicates:
        raise DagJsonError(
            "field 'integration_test_binaries' contains duplicate name "
            + repr(duplicates[0])
        )
    return values


def _write_domain_policy(value: object) -> WriteDomainPolicy:
    if value is None:
        return WriteDomainPolicy()
    obj = _as_obj(value, "write_domain_policy")
    _refuse_unknown_keys(obj, WRITE_DOMAIN_POLICY_KEYS, "write_domain_policy")
    allowed = _opt_str_list(obj, "allowed_domains")
    duplicates = sorted({name for name in allowed if allowed.count(name) > 1})
    if duplicates:
        raise DagJsonError(
            "write_domain_policy.allowed_domains contains duplicates: " + ", ".join(duplicates)
        )
    if any(not name for name in allowed):
        raise DagJsonError("write_domain_policy.allowed_domains must not contain empty names")
    return WriteDomainPolicy(
        require_explicit=_opt_bool(obj, "require_explicit", False),
        allowed_domains=frozenset(allowed),
    )


def _opt_str_int_map(m: Mapping[str, object], key: str, where: str) -> dict[str, int]:
    val = m.get(key)
    if val is None:
        return {}
    obj = _as_obj(val, f"{where}.{key}")
    out: dict[str, int] = {}
    for name, num in obj.items():
        if isinstance(num, bool) or not isinstance(num, int):
            raise DagJsonError(f"{where}.{key}.{name}: must be an integer")
        out[name] = _check_i64(num, f"{where}.{key}.{name}")
    return out


def _opt_str_str_map(m: Mapping[str, object], key: str, where: str) -> dict[str, str]:
    val = m.get(key)
    if val is None:
        return {}
    obj = _as_obj(val, f"{where}.{key}")
    out: dict[str, str] = {}
    for name, text in obj.items():
        if not isinstance(text, str):
            raise DagJsonError(f"{where}.{key}.{name}: must be a string")
        out[name] = text
    return out


def _hint_from(value: object, where: str) -> ResourceHint:
    if value is None:
        return ResourceHint()
    obj = _as_obj(value, where)
    _refuse_unknown_keys(obj, HINT_KEYS, where)
    cls_name = _opt_str(obj, "classification", StepClass.LIGHT.value)
    try:
        classification = StepClass(cls_name)
    except ValueError as exc:
        raise DagJsonError(f"{where}.classification: unknown value {cls_name!r}") from exc
    return ResourceHint(
        resources=_opt_str_int_map(obj, "resources", where),
        est_duration_s=_opt_float(obj, "est_duration_s", 0.0),
        rss_baseline_bytes=_opt_int_or_none(obj, "rss_baseline_bytes"),
        hard_mem_max_bytes=_opt_int_or_none(obj, "hard_mem_max_bytes"),
        classification=classification,
        preferred_inner_jobs=_opt_int_or_none(obj, "preferred_inner_jobs"),
        measured_effective_cores=_opt_float_or_none(obj, "measured_effective_cores"),
        measured_cpu_utilization=_opt_float_or_none(obj, "measured_cpu_utilization"),
    )


def _reject_json_constant(token: str) -> NoReturn:
    """Reject JSON's non-standard ``Infinity`` / ``-Infinity`` / ``NaN`` literals.

    Python's ``json`` accepts these by default, but the Rust build's ``serde_json`` rejects them
    ("expected value"), so accepting them here would break byte-for-byte parity and produce
    non-round-trippable output. Wired in as ``json.loads(parse_constant=...)``.
    """
    raise DagJsonError(f"invalid JSON: non-finite float literal {token!r} is not allowed")


def _raw_from_json(text: str) -> object:
    """Parse JSON without narrowing it to a DAG yet."""
    try:
        return json.loads(text, parse_constant=_reject_json_constant)
    except json.JSONDecodeError as exc:
        raise DagJsonError(f"invalid JSON: {exc}") from exc


def dag_from_json(text: str) -> DagConfig:
    """Parse one self-contained JSON DAG.

    ``include`` is path-relative source syntax, so it is deliberately refused by this
    context-free parser. Use :func:`dag_from_path` for a composed DAG.
    """
    raw = _raw_from_json(text)
    return _dag_from_obj(raw)


# --- YAML plain-scalar resolution matched to the Rust build (serde_norway / YAML 1.2 core) ---
#
# PyYAML defaults to YAML 1.1 implicit typing, which resolves UNQUOTED scalars differently from
# serde_norway (the Rust build's YAML parser, ~YAML 1.2 core schema). YAML 1.1 turns `no`/`yes`/
# `on`/`off` into booleans (the "Norway problem"), `0755` into octal, `1_000`/`1:20` into numbers,
# and `2024-01-01` into a date — none of which serde_norway does. Left unaddressed, the SAME .yaml
# would load to a different model in one build, or be accepted by one and rejected by the other:
# exactly the desync a two-language runner must never have. So PLAIN (unquoted) scalars are
# resolved here with the SAME core-schema rules serde_norway uses; quoted and block scalars always
# stay strings. cross/differential.py pins this against the Rust build token-for-token.

_RE_INT_DEC = re.compile(r"^[-+]?(0|[1-9][0-9]*)$")
_RE_INT_OCT = re.compile(r"^0o[0-7]+$")
_RE_INT_HEX = re.compile(r"^0x[0-9a-fA-F]+$")
_RE_INT_BIN = re.compile(r"^0b[01]+$")
# A float needs a '.' or an exponent; a bare digit run is an int candidate, and a leading-zero
# decimal like `0755` matches nothing here and stays a STRING — matching serde_norway (which, like
# JSON, rejects leading-zero decimals) rather than YAML 1.1 (which reads it as octal).
_RE_FLOAT = re.compile(
    r"^[-+]?(\.[0-9]+|[0-9]+\.[0-9]*)([eE][-+]?[0-9]+)?$"
    r"|^[-+]?[0-9]+[eE][-+]?[0-9]+$"
)
_NULL_TOKENS = frozenset({"", "~", "null", "Null", "NULL"})
_TRUE_TOKENS = frozenset({"true", "True", "TRUE"})
_FALSE_TOKENS = frozenset({"false", "False", "FALSE"})
# serde_norway recognizes these as non-finite floats, but serde_json::Value cannot hold a
# non-finite number, so it stores null. We mirror that: a non-finite scalar becomes null (not an
# error), so a non-nullable float field rejects it on BOTH sides and a nullable one reads null on
# both — never an accepted infinity.
_NONFINITE_TOKENS = frozenset(
    {
        ".inf", ".Inf", ".INF", "+.inf", "+.Inf", "+.INF", "-.inf", "-.Inf", "-.INF",
        ".nan", ".NaN", ".NAN",
    }
)

_CORE_TAG = "tag:agent-utils,2026:core-scalar"


def _resolve_core_scalar(value: str) -> object:
    """Resolve one PLAIN YAML scalar to the same type serde_norway (YAML 1.2 core) would.

    Returns ``None`` / ``bool`` / ``int`` / ``float`` / ``str``. Leading-zero decimals (``0755``),
    underscore grouping (``1_000``), sexagesimal (``1:20``), and timestamps (``2024-01-01``) stay
    STRINGS; ``0o`` / ``0x`` / ``0b`` integer forms and dotted/exponent floats are parsed; a float
    that overflows to infinity (``1e400``) stays a string and an explicit non-finite token
    (``.inf`` / ``.nan``) becomes null — exactly as serde_norway does.
    """
    if value in _NULL_TOKENS:
        return None
    if value in _TRUE_TOKENS:
        return True
    if value in _FALSE_TOKENS:
        return False
    if _RE_INT_DEC.match(value):
        return int(value, 10)
    if _RE_INT_OCT.match(value):
        return int(value[2:], 8)
    if _RE_INT_HEX.match(value):
        return int(value[2:], 16)
    if _RE_INT_BIN.match(value):
        return int(value[2:], 2)
    if value in _NONFINITE_TOKENS:
        return None
    if _RE_FLOAT.match(value):
        parsed = float(value)
        return parsed if math.isfinite(parsed) else value
    return value


def _construct_core_scalar(loader: yaml.SafeLoader, node: ScalarNode) -> object:
    return _resolve_core_scalar(loader.construct_scalar(node))


# Cache for the lazily-built loader subclass. PyYAML is imported and the subclass defined only on
# the first YAML load (see _core_loader), so JSON-only paths and `--help` never touch yaml.
_CORE_LOADER: type[yaml.SafeLoader] | None = None


def _core_loader() -> type[yaml.SafeLoader]:
    """Build (once, lazily) the ``SafeLoader`` that resolves PLAIN scalars via core-schema rules.

    Every PLAIN (unquoted) scalar is routed to :func:`_construct_core_scalar` via a single
    catch-all implicit resolver that REPLACES PyYAML's default YAML-1.1 implicit typing. Quoted and
    block scalars keep the default string tag, so a quoted ``"no"`` stays the string ``"no"`` and a
    plain ``no`` also stays a string — matching serde_norway and never resurrecting the Norway
    problem. Defining the subclass here avoids importing PyYAML on JSON-only paths.
    """
    global _CORE_LOADER
    if _CORE_LOADER is not None:
        return _CORE_LOADER
    import yaml

    class _CoreLoader(yaml.SafeLoader):
        pass

    # Replace PyYAML's implicit resolvers with ONE catch-all under the wildcard (``None``)
    # first-char key: every plain scalar therefore resolves to ``_CORE_TAG`` and is handed to our
    # core-schema constructor, while quoted/block scalars fall through to the default string tag.
    # Assigned directly (not via the untyped ``add_implicit_resolver`` classmethod) so this stays
    # mypy-strict clean, and only on the subclass so PyYAML's shared ``SafeLoader`` table is left
    # untouched.
    _CoreLoader.yaml_implicit_resolvers = {None: [(_CORE_TAG, re.compile(r".*", re.DOTALL))]}
    _CoreLoader.add_constructor(_CORE_TAG, _construct_core_scalar)
    _CORE_LOADER = _CoreLoader
    return _CoreLoader


def _raw_from_yaml(text: str) -> object:
    """Parse YAML without narrowing it to a DAG yet."""
    try:
        import yaml
    except ModuleNotFoundError as exc:
        raise DagJsonError(_MISSING_YAML_MSG) from exc
    try:
        # yaml.load returns Any; pin it to `object` at the parse boundary so no Any leaks past here
        # (the strict narrowing in _dag_from_obj re-validates every field's type anyway).
        return yaml.load(text, Loader=_core_loader())
    except yaml.YAMLError as exc:
        # Match the Rust build, which returns a load error (exit 2) on malformed YAML rather than
        # letting an exception escape.
        raise DagJsonError(f"invalid YAML: {exc}") from exc


def dag_from_yaml(text: str) -> DagConfig:
    """Parse one self-contained YAML DAG.

    YAML uses the same strict schema as JSON, with YAML 1.2 core scalar resolution.
    Comments and multi-line block scalars are accepted. ``include`` requires filesystem
    context and is therefore accepted only by :func:`dag_from_path`.
    """
    return _dag_from_obj(_raw_from_yaml(text))


# ``include`` is SOURCE syntax. It is consumed while loading files and never becomes part of the
# runtime model or canonical JSON. Keeping composition out of DagConfig means every downstream
# command sees the same ordinary, flattened graph as if it had been written in one file.
_INCLUDE_KEYS = frozenset({"after", "namespace", "path"})
_NAMESPACE_SEGMENT = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]*$")
_GLOBAL_POLICY_KEYS = (
    "mem_cap_factor",
    "mem_cap_floor_bytes",
    "outer_mem_safety_factor",
    "default_step_timeout",
    "default_jobs_flag",
    "default_jobs_env",
    "write_domain_policy",
)


def _include_entries(
    doc: Mapping[str, object], source: str
) -> list[tuple[str, str, list[str]]]:
    """Strictly parse the reserved top-level ``include`` key."""
    if "include" not in doc:
        return []
    raw = doc["include"]
    if not isinstance(raw, list):
        raise DagJsonError(f"{source}.include: must be a list")
    entries: list[tuple[str, str, list[str]]] = []
    for index, value in enumerate(raw):
        where = f"{source}.include[{index}]"
        entry = _as_obj(value, where)
        _refuse_unknown_keys(entry, _INCLUDE_KEYS, where)
        path = _req_str(entry, "path", where)
        namespace = _req_str(entry, "namespace", where)
        if not path:
            raise DagJsonError(f"{where}.path: must be non-empty")
        if not _NAMESPACE_SEGMENT.fullmatch(namespace):
            raise DagJsonError(
                f"{where}.namespace: must match [A-Za-z0-9][A-Za-z0-9_-]*"
            )
        raw_after = entry.get("after", [])
        if not isinstance(raw_after, list):
            raise DagJsonError(f"{where}.after: must be a list of strings")
        after: list[str] = []
        for tag in raw_after:
            if not isinstance(tag, str):
                raise DagJsonError(f"{where}.after: must contain only strings")
            after.append(tag)
        if any(not tag for tag in after):
            raise DagJsonError(f"{where}.after: tags must be non-empty")
        if len(after) != len(set(after)):
            raise DagJsonError(f"{where}.after: duplicate tags are not allowed")
        entries.append((path, namespace, after))
    return entries


def _qualify(namespace: str, value: str) -> str:
    return f"{namespace}.{value}" if namespace else value


def _rewrite_step(entry: object, namespace: str) -> object:
    """Copy one raw step and qualify every field whose value is a step identity."""
    rewritten = deepcopy(entry)
    if not namespace or not isinstance(rewritten, dict):
        return rewritten

    group = rewritten.get("group")
    if isinstance(group, str):
        rewritten["group"] = _qualify(namespace, group)
    for key in ("deps", "explains"):
        values = rewritten.get(key)
        if isinstance(values, list):
            rewritten[key] = [
                _qualify(namespace, value) if isinstance(value, str) else value
                for value in values
            ]
    family = rewritten.get("fail_fast_family")
    if isinstance(family, str) and family.strip():
        rewritten["fail_fast_family"] = _qualify(namespace, family)
    declarations = rewritten.get("result_manifests")
    if isinstance(declarations, list):
        for declaration in declarations:
            if (
                isinstance(declaration, dict)
                and declaration.get("kind") == STRUCTURED_TEST_RESULTS_KIND
                and isinstance(declaration.get("owner"), str)
                and declaration["owner"]
            ):
                declaration["owner"] = _qualify(namespace, declaration["owner"])
    return rewritten


def _policy_config(doc: Mapping[str, object]) -> DagConfig:
    """Parse only document-global policy, independently of fragment-local steps."""
    policy_doc = deepcopy(dict(doc))
    policy_doc.pop("include", None)
    policy_doc["steps"] = []
    return _dag_from_obj(policy_doc)


def _policy_value(config: DagConfig, key: str) -> object:
    return getattr(config, key)


def _read_dag_source(path: Path, *, syntax_path: Path | None = None) -> dict[str, object]:
    """Read canonical ``path`` using the extension of the path the author named."""
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        raise DagJsonError(f"{path}: cannot read DAG: {exc}") from exc
    try:
        raw = (
            _raw_from_yaml(text)
            if (syntax_path or path).suffix.lower() in {".yaml", ".yml"}
            else _raw_from_json(text)
        )
        return _as_obj(raw, "<root>")
    except DagJsonError as exc:
        detail = str(exc).removeprefix("<root>: ")
        raise DagJsonError(f"{path}: {detail}") from exc


def _resolve_include_path(raw: str, source: Path, root_dir: Path) -> Path:
    candidate = Path(raw)
    if candidate.is_absolute():
        raise DagJsonError(f"{source}: include path {raw!r} must be relative")
    if (
        re.match(r"^[A-Za-z][A-Za-z0-9+.-]*://", raw)
        or raw.startswith("~")
        or "$" in raw
        or any(marker in raw for marker in ("*", "?", "["))
    ):
        raise DagJsonError(
            f"{source}: include path {raw!r} must be a literal relative path "
            "(URLs, globs, '~', and environment expansion are not supported)"
        )
    try:
        resolved = (source.parent / candidate).resolve(strict=True)
    except (OSError, RuntimeError, ValueError) as exc:
        raise DagJsonError(f"{source}: cannot resolve include path {raw!r}: {exc}") from exc
    try:
        resolved.relative_to(root_dir)
    except ValueError as exc:
        raise DagJsonError(
            f"{source}: include path {raw!r} resolves outside top-level DAG directory {root_dir}"
        ) from exc
    if not resolved.is_file():
        raise DagJsonError(f"{source}: include path {raw!r} does not name a regular file")
    return resolved


def dag_from_path(path: str | Path) -> DagConfig:
    """Load and flatten a JSON/YAML DAG plus its namespaced includes.

    Included paths are relative to their declaring file and must remain below the top-level DAG
    directory after symlink resolution. Includes are visited depth-first in declaration order;
    each included file's local steps follow its own children, and the top-level file's local steps
    come last. A canonical file may be reused under different namespaces, never twice under the
    same effective namespace. An include entry's optional ``after`` tags are resolved in the
    parent's namespace and injected into every entry node of the included flattened subgraph.
    """
    requested = Path(path)
    try:
        root = requested.resolve(strict=True)
    except (OSError, RuntimeError, ValueError) as exc:
        raise DagJsonError(f"{requested}: cannot read DAG: {exc}") from exc
    if not root.is_file():
        raise DagJsonError(f"{requested}: does not name a regular file")
    root_dir = root.parent
    root_doc = _read_dag_source(root, syntax_path=requested)
    _include_entries(root_doc, str(root))
    try:
        root_policy = _policy_config(root_doc)
    except DagJsonError as exc:
        raise DagJsonError(f"{root}: {str(exc).removeprefix('<root>: ')}") from exc

    flattened_steps: list[object] = []
    resource_caps: dict[str, int] = {}
    cap_sources: dict[str, Path] = {}
    step_sources: dict[str, tuple[Path, int, str]] = {}
    seen: set[tuple[Path, str]] = set()
    active: list[Path] = []
    include_instances = 0
    declared_steps = 0
    after_references: list[tuple[str, Path]] = []

    def inject_after(start: int, end: int, after: list[str], source: Path) -> None:
        if not after:
            return
        subgraph_tags = {
            f"{entry['group']}.{entry['job']}"
            for entry in flattened_steps[start:end]
            if isinstance(entry, dict)
            and isinstance(entry.get("group"), str)
            and isinstance(entry.get("job"), str)
        }
        internal = sorted(set(after) & subgraph_tags)
        if internal:
            raise DagJsonError(
                f"{source}: include after must name outer steps, not tag(s) inside the "
                f"included subgraph: {', '.join(internal)}"
            )
        for entry in flattened_steps[start:end]:
            if not isinstance(entry, dict):
                continue
            deps = entry.get("deps")
            if deps is None:
                deps = []
            if not isinstance(deps, list) or not all(isinstance(dep, str) for dep in deps):
                continue
            if any(dep in subgraph_tags for dep in deps):
                continue
            entry["deps"] = deps + [tag for tag in after if tag not in deps]

    def visit(
        source: Path,
        namespace: str,
        document: dict[str, object] | None = None,
        *,
        syntax_path: Path | None = None,
        depth: int = 0,
    ) -> tuple[int, int]:
        nonlocal declared_steps, include_instances
        if source in active:
            start = active.index(source)
            chain = active[start:] + [source]
            raise DagJsonError("include cycle: " + " -> ".join(str(item) for item in chain))
        identity = (source, namespace)
        if identity in seen:
            display = namespace or "<root>"
            raise DagJsonError(
                f"duplicate include: {source} is loaded more than once as namespace {display!r}"
            )
        seen.add(identity)
        active.append(source)
        start = len(flattened_steps)
        try:
            doc = (
                document
                if document is not None
                else _read_dag_source(source, syntax_path=syntax_path)
            )
            try:
                _refuse_uncarried_config_keys(doc)
                includes = _include_entries(doc, str(source))
                steps = doc.get("steps")
                if not isinstance(steps, list):
                    raise _steps_list_error(doc)
                declared_steps += len(steps)
                if declared_steps > MAX_DAG_FLATTENED_STEPS:
                    raise DagJsonError(
                        f"composed DAG exceeds {MAX_DAG_FLATTENED_STEPS} flattened steps"
                    )
                fragment_policy = _policy_config(doc)
            except DagJsonError as exc:
                detail = str(exc).removeprefix("<root>: ")
                if detail.startswith(str(source)):
                    raise
                raise DagJsonError(f"{source}: {detail}") from exc

            if source != root:
                for key in _GLOBAL_POLICY_KEYS:
                    if key in doc and _policy_value(fragment_policy, key) != _policy_value(
                        root_policy, key
                    ):
                        raise DagJsonError(
                            f"{source}: global policy {key!r} conflicts with top-level DAG {root}"
                        )

            caps = _opt_str_int_map(doc, "resource_caps", str(source))
            for name, value in sorted(caps.items()):
                if name in resource_caps and resource_caps[name] != value:
                    raise DagJsonError(
                        f"{source}: resource cap {name!r}={value} conflicts with "
                        f"{cap_sources[name]} value {resource_caps[name]}"
                    )
                resource_caps[name] = value
                cap_sources.setdefault(name, source)

            for raw_path, segment, after in includes:
                if depth >= MAX_DAG_INCLUDE_DEPTH:
                    raise DagJsonError(
                        f"{source}: include nesting exceeds maximum depth "
                        f"{MAX_DAG_INCLUDE_DEPTH}"
                    )
                if include_instances >= MAX_DAG_INCLUDE_INSTANCES:
                    raise DagJsonError(
                        f"{source}: composed DAG exceeds {MAX_DAG_INCLUDE_INSTANCES} "
                        "include instances"
                    )
                include_instances += 1
                included = _resolve_include_path(raw_path, source, root_dir)
                child_namespace = _qualify(namespace, segment)
                child_start, child_end = visit(
                    included,
                    child_namespace,
                    syntax_path=Path(raw_path),
                    depth=depth + 1,
                )
                qualified_after = [_qualify(namespace, tag) for tag in after]
                after_references.extend((tag, source) for tag in qualified_after)
                inject_after(
                    child_start,
                    child_end,
                    qualified_after,
                    source,
                )

            for index, entry in enumerate(steps):
                rewritten = _rewrite_step(entry, namespace)
                if isinstance(rewritten, dict):
                    group, job = rewritten.get("group"), rewritten.get("job")
                    if isinstance(group, str) and isinstance(job, str):
                        tag = f"{group}.{job}"
                        prior = step_sources.get(tag)
                        if prior is not None:
                            prior_path, prior_index, prior_namespace = prior
                            if not (
                                source == root
                                and prior_path == root
                                and not namespace
                                and not prior_namespace
                            ):
                                raise DagJsonError(
                                    f"duplicate step tag {tag!r}: {prior_path} "
                                    f"steps[{prior_index}] (namespace "
                                    f"{prior_namespace or '<root>'!r}) and {source} "
                                    f"steps[{index}] (namespace {namespace or '<root>'!r})"
                                )
                        else:
                            step_sources[tag] = (source, index, namespace)
                flattened_steps.append(rewritten)
            return start, len(flattened_steps)
        finally:
            active.pop()

    visit(root, "", root_doc)
    for tag, source in after_references:
        if tag not in step_sources:
            raise DagJsonError(
                f"{source}: include after references missing outer step {tag!r}"
            )
    flattened = deepcopy(root_doc)
    flattened.pop("include", None)
    flattened["steps"] = flattened_steps
    flattened["resource_caps"] = resource_caps
    try:
        return _dag_from_obj(flattened)
    except DagJsonError as exc:
        raise DagJsonError(f"{root}: {str(exc).removeprefix('<root>: ')}") from exc


#: :class:`DagConfig` fields that the DOCUMENT FORMAT deliberately does not carry.
#:
#: Writing one of these at the top level of a DAG file today has no effect whatsoever: the parser
#: never looks at the key, the field takes its dataclass default, and nothing says so.  That is
#: the dropped-field bug from the reader's side — a configured cap silently replaced by a default,
#: with no report — so the loader REFUSES the key by name instead of ignoring it.
#:
#: Genuinely unknown keys stay tolerated: a key nobody has ever implemented cannot masquerade as a
#: setting that took effect, whereas one that names a real field reads exactly like one that did.
#: ``known_failures`` is listed although only this edition has the field, so both editions refuse
#: the same set of keys byte for byte.
UNCARRIED_CONFIG_KEYS: tuple[str, ...] = (
    "default_step_mem_cap_bytes",
    "default_step_cpu_count",
    "default_step_cpu_timeout",
    "cpu_timeout_multiplier",
    "cpu_timeout_platform",
    "known_failures",
)


def _refuse_uncarried_config_keys(doc: Mapping[str, object]) -> None:
    """Raise when the document sets a real config field the format cannot carry."""
    present = [key for key in UNCARRIED_CONFIG_KEYS if key in doc]
    if not present:
        return
    raise DagJsonError(
        f"<root>: {len(present)} top-level key(s) name a DagConfig field the DAG document "
        "format does not carry, so the value would be SILENTLY replaced by a default: "
        + ", ".join(present)
        + ". Set these on the DagConfig at the call site (they are caller/platform policy, not "
        "properties of the graph), or remove them."
    )


#: Every key a ``steps[]`` entry may carry. The step object is CLOSED.
#:
#: The top level is deliberately open (see :data:`UNCARRIED_CONFIG_KEYS`): a top-level key nobody
#: has ever implemented cannot masquerade as a setting that took effect, so tolerating it buys
#: forward compatibility at no cost. A STEP key is the opposite case. Every one of them is a
#: per-node instruction, and the near-misses are all real spellings a person writes by accident --
#: ``dep``, ``cmds``, ``timeouts``, ``env_vars``, ``description`` vs ``desc``. Silently ignored,
#: the instruction is simply not carried out and the document still says it was: the step runs
#: with no timeout, no dependency, no environment, and nothing anywhere reports it.
STEP_KEYS: frozenset[str] = frozenset(
    {
        "group",
        "job",
        "desc",
        "description",
        "labels",
        "cmd",
        "cmdtype",
        "deps",
        "env",
        "hint",
        "integration_test_binaries",
        "networkonly",
        "engine_only",
        "delegated_children",
        "timeout",
        "cpu_timeout",
        "jobs_flag",
        "jobs_env",
        "skip_reason",
        "write_domains",
        "write_domain_guarantee",
        "explains",
        "fail_fast_family",
        # ⚠️ DECLARED HERE, CONSUMED DOWNSTREAM, NOT BY dagrun.
        # `requires_host_capability` drives a consuming planner's
        # HOST-INAPPLICABLE decision, while `manifest`, `result_manifests`, and
        # `integration_test_binaries` carry other consumer-owned selection facts.
        # dagrun retains the declarations; only its generic exact-result ownership
        # helper interprets the selectors.
        #
        # Retained because this schema is CLOSED, and closing it without these fields
        # made dagrun REFUSE graphs that were already in use: measured
        # 2026-08-26, `dagrun list` exited 2 on `manifest` for one real graph and
        # on `requires_host_capability` for another. A closed schema that does not
        # know its callers' fields does not raise the bar, it takes the caller's
        # validate offline.
        "manifest",
        "result_manifests",
        "requires_host_capability",
    }
)

#: Every key a step's ``hint`` object may carry. Closed for the same reason as
#: :data:`STEP_KEYS`; ``est_duration`` for ``est_duration_s`` is the canonical silent drop.
HINT_KEYS: frozenset[str] = frozenset(
    {
        "resources",
        "est_duration_s",
        "rss_baseline_bytes",
        "hard_mem_max_bytes",
        "classification",
        "preferred_inner_jobs",
        "measured_effective_cores",
        "measured_cpu_utilization",
    }
)

MANIFEST_KEYS: frozenset[str] = frozenset({"lane", "category", "test", "mode", "backend"})

STRUCTURED_TEST_RESULTS_KEYS = frozenset({"kind", "schema", "path_env", "owner"})

def _manifest_value_from(value: object, where: str) -> DagManifest:
    obj = _as_obj(value, where)
    _refuse_unknown_keys(obj, MANIFEST_KEYS, where)
    lane = _req_str(obj, "lane", where)
    category = _req_str(obj, "category", where)
    if not lane:
        raise DagJsonError(f"{where}.lane: must be non-empty")
    if not category:
        raise DagJsonError(f"{where}.category: must be non-empty")
    test = _opt_str_or_none(obj, "test")
    mode = _opt_str_or_none(obj, "mode")
    backend = _opt_str_or_none(obj, "backend")
    for field, selected in (("test", test), ("mode", mode), ("backend", backend)):
        if selected == "":
            raise DagJsonError(f"{where}.{field}: must be non-empty when present")
    return DagManifest(lane=lane, category=category, test=test, mode=mode, backend=backend)


def _manifest_from(value: object, where: str) -> DagManifest | None:
    if value is None:
        return None
    return _manifest_value_from(value, where)


def _result_manifest_value_from(value: object, where: str) -> ResultManifest:
    obj = _as_obj(value, where)
    if "kind" not in obj:
        return _manifest_value_from(value, where)
    _refuse_unknown_keys(obj, STRUCTURED_TEST_RESULTS_KEYS, where)
    kind = _req_str(obj, "kind", where)
    if kind != STRUCTURED_TEST_RESULTS_KIND:
        raise DagJsonError(f"{where}.kind: unknown result-manifest kind {kind!r}")
    schema = obj.get("schema")
    if isinstance(schema, bool) or not isinstance(schema, int):
        raise DagJsonError(f"{where}.schema: must be an integer")
    if schema not in {
        STRUCTURED_TEST_RESULTS_CURRENT_SCHEMA,
        STRUCTURED_TEST_RESULTS_CLASSIFIED_SCHEMA,
        STRUCTURED_TEST_RESULTS_DIAGNOSTIC_SCHEMA,
    }:
        raise DagJsonError(
            f"{where}.schema: structured test results require default schema "
            f"{STRUCTURED_TEST_RESULTS_CURRENT_SCHEMA}, classified schema "
            f"{STRUCTURED_TEST_RESULTS_CLASSIFIED_SCHEMA} or diagnostic schema "
            f"{STRUCTURED_TEST_RESULTS_DIAGNOSTIC_SCHEMA}, got {schema}"
        )
    path_env = _req_str(obj, "path_env", where)
    if path_env != STRUCTURED_TEST_RESULTS_PATH_ENV:
        raise DagJsonError(
            f"{where}.path_env: structured test results require "
            f"{STRUCTURED_TEST_RESULTS_PATH_ENV!r}, got {path_env!r}"
        )
    owner = _req_str(obj, "owner", where)
    if not owner:
        raise DagJsonError(f"{where}.owner: must be non-empty")
    if schema == STRUCTURED_TEST_RESULTS_CURRENT_SCHEMA:
        return StructuredTestResultsManifest.current(owner)
    if schema == STRUCTURED_TEST_RESULTS_DIAGNOSTIC_SCHEMA:
        return StructuredTestResultsManifest.diagnostic(owner)
    return StructuredTestResultsManifest.classified(owner)


def _result_manifests_from(value: object, where: str) -> list[ResultManifest] | None:
    if value is None:
        return None
    if not isinstance(value, list):
        raise DagJsonError(f"{where}: must be a list of result declarations or null")
    manifests: list[ResultManifest] = []
    for index, item in enumerate(value):
        manifest = _result_manifest_value_from(item, f"{where}[{index}]")
        if manifest in manifests:
            raise DagJsonError(f"{where}: duplicate selector at index {index}")
        manifests.append(manifest)
    return manifests

#: Every key the top-level ``write_domain_policy`` object may carry. Closed: a misspelled
#: ``require_explicit`` turns a fail-closed policy into no policy at all, silently.
WRITE_DOMAIN_POLICY_KEYS: frozenset[str] = frozenset({"require_explicit", "allowed_domains"})


def _refuse_unknown_keys(
    m: Mapping[str, object], known: frozenset[str], where: str
) -> None:
    """Refuse keys a CLOSED schema object does not define, naming every one of them.

    ``resources`` and ``env`` are caller-defined key spaces and are never narrowed here; only the
    schema objects themselves are closed.
    """
    unknown = sorted(key for key in m if key not in known)
    if not unknown:
        return
    raise DagJsonError(
        f"{where}: unknown field(s) {', '.join(repr(key) for key in unknown)}. "
        "This object's schema is CLOSED, because an ignored field reads exactly like one that "
        "took effect. Known fields: " + ", ".join(sorted(known))
    )


def _refuse_unusable_explains(steps: Sequence[Step]) -> None:
    """Refuse an ``explains`` declaration that cannot mean what it says.

    ``explains`` buys a step an exemption from eager-exit cancellation, so a declaration that is
    quietly wrong is worse than one that is missing: the step looks protected in the document and
    is reaped anyway, or protects itself for a reason nobody can audit. Each refusal below is a
    way that can happen.

    UNKNOWN TAG -- names a node that does not exist, so the exemption can never trigger. This is
    the misspelling case, and it is silent without a check: the field parses, the step schedules
    normally, and the protection simply never applies.

    SELF-REFERENCE -- a step cannot explain its own failure. Permitting it would let any step opt
    itself out of cancellation with a single self-naming line, which is precisely the blanket
    opt-out this relationship exists to avoid.

    CYCLE -- A explains B and B explains A (directly or through a chain). Each would then shield
    the other, so the pair becomes mutually uncancellable and eager-exit silently stops applying
    to it. Nothing reports that; the run just gets slower and the pact is invisible in the
    document because each individual line looks reasonable. Refusing cycles keeps the relation a
    strict "diagnoses" hierarchy, which is the only reading under which the exemption is bounded.
    """
    by_tag = {step.tag: step for step in steps}
    problems: list[str] = []
    for step in steps:
        for target in step.explains:
            if target == step.tag:
                problems.append(f"step {step.tag}: explains itself; a step cannot diagnose its own failure")
            elif target not in by_tag:
                problems.append(f"step {step.tag}: explains unknown node {target!r}")
    if problems:
        raise DagJsonError("; ".join(sorted(set(problems))))

    # Iterative three-colour DFS over the explains relation. Iterative rather than recursive to
    # match how this repository walks graphs elsewhere, so a deep chain cannot hit the recursion
    # limit and turn a validation error into a crash.
    WHITE, GREY, BLACK = 0, 1, 2
    colour = {tag: WHITE for tag in by_tag}
    for root in sorted(by_tag):
        if colour[root] != WHITE:
            continue
        stack: list[tuple[str, bool]] = [(root, False)]
        path: list[str] = []
        while stack:
            tag, leaving = stack.pop()
            if leaving:
                colour[tag] = BLACK
                path.pop()
                continue
            if colour[tag] == BLACK:
                continue
            if colour[tag] == GREY:
                cycle = path[path.index(tag):] + [tag]
                raise DagJsonError(
                    "explains cycle: " + " -> ".join(cycle) + ". Each step in the cycle would "
                    "exempt the next from eager-exit, so the whole cycle becomes uncancellable "
                    "and eager-exit silently stops applying to it. `explains` must describe a "
                    "one-way 'diagnoses' relation."
                )
            colour[tag] = GREY
            path.append(tag)
            stack.append((tag, True))
            for target in sorted(by_tag[tag].explains):
                if colour.get(target) != BLACK:
                    stack.append((target, False))


def _dag_from_obj(raw: object) -> DagConfig:
    """Build a :class:`DagConfig` from an already-parsed JSON/YAML object.

    The shared strict narrowing behind both :func:`dag_from_json` and :func:`dag_from_yaml`, so the
    two syntaxes cannot drift in how they construct the model.
    """
    doc = _as_obj(raw, "<root>")
    _refuse_uncarried_config_keys(doc)
    # `include` is reserved syntax, not an open-schema extension. Validate it first so a typo in
    # an entry is never hidden behind the generic context error, then require the path-aware API.
    _include_entries(doc, "<root>")
    if "include" in doc:
        raise DagJsonError(
            "<root>.include: includes require filesystem context; load this document with "
            "dag_from_path() or pass its filename to --dag (includes cannot be read from stdin)"
        )
    # The document-level default_step_timeout is the per-step default for any step that omits
    # its own `timeout`. Parse it BEFORE the step loop so it can be threaded in as each step's
    # default. ABSENT IS NOT 1800: an omitted default leaves both it and the step at the 0
    # sentinel, and `resolved_wall_timeout` derives the bound from the step's declared CPU budget
    # (or falls back to DEFAULT_STEP_TIMEOUT). Materializing 1800 here is what baked the
    # load-sensitive number into every graph.
    default_step_timeout = _opt_int(doc, "default_step_timeout", 0)
    steps_raw = doc.get("steps")
    if not isinstance(steps_raw, list):
        raise _steps_list_error(doc)
    policy = _write_domain_policy(doc.get("write_domain_policy"))
    steps: list[Step] = []
    for i, entry in enumerate(steps_raw):
        where = f"steps[{i}]"
        sm = _as_obj(entry, where)
        # Name the offending step by TAG as well as by index wherever the document supplies one:
        # "steps[7]" sends a reader counting entries in a 200-step file. Only the closed-schema
        # refusal gets the decorated location; the per-field type errors keep their established
        # "steps[N].field" spelling.
        group_val, job_val = sm.get("group"), sm.get("job")
        named = (
            f"{where} ({group_val}.{job_val})"
            if isinstance(group_val, str) and isinstance(job_val, str)
            else where
        )
        _refuse_unknown_keys(sm, STEP_KEYS, named)
        skip_text = _opt_str_or_none(sm, "skip_reason")
        try:
            skip_reason = IntentionalSkipReason(skip_text) if skip_text is not None else None
        except ValueError as exc:
            raise DagJsonError(
                f"{where}.skip_reason: unknown value {skip_text!r}"
            ) from exc
        fail_fast_family = _opt_str_or_none(sm, "fail_fast_family")
        if fail_fast_family is not None and not fail_fast_family.strip():
            raise DagJsonError(f"{where}.fail_fast_family: must be non-empty")
        steps.append(
            Step(
                group=_req_str(sm, "group", where),
                job=_req_str(sm, "job", where),
                desc=_opt_str(sm, "desc", ""),
                description=_opt_str(sm, "description", ""),
                cmd=_req_str(sm, "cmd", where),
                labels=_labels(sm, where),
                manifest=_manifest_from(sm.get("manifest"), f"{where}.manifest"),
                result_manifests=_result_manifests_from(
                    sm.get("result_manifests"), f"{where}.result_manifests"
                ),
                integration_test_binaries=_integration_test_binaries(sm),
                cmdtype=_cmdtype_field(sm, where),
                deps=_opt_str_list(sm, "deps"),
                env=_opt_str_str_map(sm, "env", where),
                hint=_hint_from(sm.get("hint"), f"{where}.hint"),
                networkonly=_opt_bool(sm, "networkonly", False),
                engine_only=_opt_bool(sm, "engine_only", False),
                delegated_children=_opt_bool(sm, "delegated_children", False),
                timeout=_opt_int(sm, "timeout", default_step_timeout),
                cpu_timeout=_opt_int(sm, "cpu_timeout", 0),
                jobs_flag=_opt_str_or_none(sm, "jobs_flag"),
                jobs_env=_jobs_env_field(sm, "jobs_env", where),
                skip_reason=skip_reason,
                explains=_opt_str_list(sm, "explains"),
                fail_fast_family=fail_fast_family,
                write_domains=_present_str_list(sm, "write_domains"),
                write_domain_guarantee=(
                    None
                    if sm.get("write_domain_guarantee") is None
                    else _parse_write_domain_guarantee(sm, where)
                ),
            )
        )
    intentional = {step.tag for step in steps if step.skip_reason is not None}
    for step in steps:
        blocked = sorted(set(step.deps) & intentional)
        if blocked:
            raise DagJsonError(
                f"step {step.tag}: dependency on intentionally skipped node(s) "
                f"{', '.join(blocked)} is undefined"
            )
    _refuse_unusable_explains(steps)
    try:
        default_jobs_env = resolve_jobs_env(
            _opt_str(doc, "default_jobs_env", "")
            if "default_jobs_env" in doc
            else None
        )
    except ValueError as exc:
        raise DagJsonError(f"<root>.default_jobs_env: {exc}") from exc
    cfg = DagConfig(
        steps=tuple(steps),
        description=_opt_str(doc, "description", ""),
        resource_caps=_opt_str_int_map(doc, "resource_caps", "<root>"),
        mem_cap_factor=_opt_float(doc, "mem_cap_factor", 1.25),
        mem_cap_floor_bytes=_opt_int(doc, "mem_cap_floor_bytes", _DEFAULT_MEM_CAP_FLOOR),
        outer_mem_safety_factor=_opt_float(doc, "outer_mem_safety_factor", 1.0),
        default_step_timeout=default_step_timeout,
        default_jobs_flag=_opt_str(doc, "default_jobs_flag", DEFAULT_JOBS_FLAG),
        # The host-level default comes from $DAGRUN_JOBS_ENV when the document does
        # not carry an explicit value. Serialization carries the resolved value so a config
        # round-trip cannot silently lose the channel.
        default_jobs_env=default_jobs_env,
        write_domain_policy=policy,
    )
    try:
        validate_cmdtype_config(cfg)
    except ValueError as exc:
        raise DagJsonError(str(exc)) from exc
    violations = write_domain_violations(cfg)
    if violations:
        raise DagJsonError("write-domain policy refused DAG before execution: " + "; ".join(violations))
    # LAST, because it is the only check that needs the finished configuration (caps and steps
    # together) and because its findings are about the graph rather than about a field.
    structural = graph_structure_violations(cfg)
    if structural:
        raise DagJsonError(
            f"<root>: {len(structural)} graph error(s) refused before execution: "
            + "; ".join(structural)
        )
    return cfg


def _parse_write_domain_guarantee(
    step: Mapping[str, object], where: str
) -> WriteDomainGuarantee:
    raw = _req_str(step, "write_domain_guarantee", where)
    try:
        return WriteDomainGuarantee(raw)
    except ValueError as exc:
        raise DagJsonError(f"{where}.write_domain_guarantee: unknown value {raw!r}") from exc


def _hint_to_json(hint: ResourceHint) -> dict[str, object]:
    return {
        "resources": dict(sorted(hint.resources.items())),
        "est_duration_s": hint.est_duration_s,
        "rss_baseline_bytes": hint.rss_baseline_bytes,
        "hard_mem_max_bytes": hint.hard_mem_max_bytes,
        "classification": hint.classification.value,
        "preferred_inner_jobs": hint.preferred_inner_jobs,
        "measured_effective_cores": hint.measured_effective_cores,
        "measured_cpu_utilization": hint.measured_cpu_utilization,
    }


def _manifest_to_json(manifest: DagManifest) -> dict[str, object]:
    obj: dict[str, object] = {
        "lane": manifest.lane,
        "category": manifest.category,
        "test": manifest.test,
        "mode": manifest.mode,
        "backend": manifest.backend,
    }
    return {key: value for key, value in obj.items() if value is not None}

def _result_manifest_to_json(manifest: ResultManifest) -> dict[str, object]:
    if isinstance(manifest, DagManifest):
        return _manifest_to_json(manifest)
    if isinstance(manifest, StructuredTestResultsManifest):
        return {
            "kind": STRUCTURED_TEST_RESULTS_KIND,
            "schema": manifest.schema,
            "path_env": STRUCTURED_TEST_RESULTS_PATH_ENV,
            "owner": manifest.owner,
        }
    raise TypeError(f"unknown result manifest {manifest!r}")


def _step_to_json(step: Step) -> dict[str, object]:
    obj: dict[str, object] = {
        "group": step.group,
        "job": step.job,
        "desc": step.desc,
        "description": step.description,
        "labels": list(step.labels),
        "cmd": step.cmd,
        "cmdtype": step.cmdtype.value,
        "manifest": _manifest_to_json(step.manifest) if step.manifest is not None else None,
        "result_manifests": (
            [_result_manifest_to_json(manifest) for manifest in step.result_manifests]
            if step.result_manifests is not None
            else None
        ),
        "integration_test_binaries": (
            list(step.integration_test_binaries)
            if step.integration_test_binaries is not None
            else None
        ),
        "deps": list(step.deps),
        "env": dict(sorted(step.env.items())),
        "networkonly": step.networkonly,
        "engine_only": step.engine_only,
        "delegated_children": step.delegated_children,
        # Both timeout fields are emitted only when SET. 0 is the "derive it" sentinel, and
        # writing it out would read as "no wall bound" — the opposite of what it means.
        "timeout": step.timeout,
        # Emitted only when set, so existing DAGs (all cpu_timeout=0) stay byte-for-byte
        # unchanged; absence parses back to 0, keeping round-trip stable. Positioned
        # immediately after `timeout` to match the Rust serializer's key order.
        "cpu_timeout": step.cpu_timeout,
        "jobs_flag": step.jobs_flag,
        "jobs_env": step.jobs_env,
        "skip_reason": step.skip_reason.value if step.skip_reason is not None else None,
        "hint": _hint_to_json(step.hint),
    }
    if step.timeout == 0:
        del obj["timeout"]
    if step.cpu_timeout == 0:
        del obj["cpu_timeout"]
    if step.cmdtype is CmdType.UNKNOWN:
        del obj["cmdtype"]
    if not step.labels:
        del obj["labels"]
    if step.manifest is None:
        del obj["manifest"]
    if step.result_manifests is None:
        del obj["result_manifests"]
    if step.integration_test_binaries is None:
        del obj["integration_test_binaries"]
    if not step.delegated_children:
        del obj["delegated_children"]
    if step.skip_reason is None:
        del obj["skip_reason"]
    # Emitted only when declared, like write_domains below: a graph that does not use the
    # relationship keeps a byte-identical document, so adding the field cannot churn every
    # existing DAG or the cross-build byte-comparison that pins the two editions together.
    if step.explains:
        obj["explains"] = list(step.explains)
    if step.fail_fast_family is not None:
        obj["fail_fast_family"] = step.fail_fast_family
    if step.write_domains is not None:
        obj["write_domains"] = list(step.write_domains)
    if step.write_domain_guarantee is not None:
        obj["write_domain_guarantee"] = step.write_domain_guarantee.value
    return obj


def _dag_to_obj(cfg: DagConfig) -> dict[str, object]:
    """Build the canonical document dict shared by :func:`dag_to_json` and :func:`dag_to_yaml`.

    A single source of truth for the field set + key order, so the two output formats cannot drift.
    """
    obj: dict[str, object] = {
        "description": cfg.description,
        "resource_caps": dict(sorted(cfg.resource_caps.items())),
        "mem_cap_factor": cfg.mem_cap_factor,
        "mem_cap_floor_bytes": cfg.mem_cap_floor_bytes,
        "outer_mem_safety_factor": cfg.outer_mem_safety_factor,
        "default_step_timeout": cfg.default_step_timeout,
        "default_jobs_flag": cfg.default_jobs_flag,
        "default_jobs_env": cfg.default_jobs_env,
        "steps": [_step_to_json(s) for s in cfg.steps],
    }
    if cfg.default_step_timeout == 0:
        del obj["default_step_timeout"]
    policy = cfg.write_domain_policy
    if policy.require_explicit or policy.allowed_domains:
        obj["write_domain_policy"] = {
            "require_explicit": policy.require_explicit,
            "allowed_domains": sorted(policy.allowed_domains),
        }
    return obj


def dag_to_json(cfg: DagConfig) -> str:
    """Serialize a :class:`DagConfig` to deterministic, two-space-indented JSON.

    Non-ASCII characters are emitted as UTF-8, while JSON control characters, quotes,
    and backslashes are escaped according to the JSON specification.
    """
    return json.dumps(_dag_to_obj(cfg), indent=2, ensure_ascii=False)


def _represent_core_str(dumper: yaml.SafeDumper, data: str) -> ScalarNode:
    """Force-quote any string that :func:`_resolve_core_scalar` would re-read as a NON-string.

    PyYAML's default dumper decides quoting from its YAML-1.1 resolver, so it leaves e.g. ``1e3``
    or ``0o17`` (and the empty string) UNQUOTED — but :func:`_core_loader` (YAML-1.2 core) would
    then re-read those as a float / int / null. Quoting exactly those strings keeps
    :func:`dag_to_yaml` output round-trippable back through :func:`dag_from_yaml`.
    """
    style = "'" if not isinstance(_resolve_core_scalar(data), str) else None
    return dumper.represent_scalar("tag:yaml.org,2002:str", data, style=style)


# Cache for the lazily-built dumper subclass; see _core_dumper.
_CORE_DUMPER: type[yaml.SafeDumper] | None = None


def _core_dumper() -> type[yaml.SafeDumper]:
    """Build (once, lazily) the ``SafeDumper`` whose string representer round-trips through
    :func:`_core_loader`. Defined here (not at module scope) to keep PyYAML optional."""
    global _CORE_DUMPER
    if _CORE_DUMPER is not None:
        return _CORE_DUMPER
    import yaml

    class _CoreDumper(yaml.SafeDumper):
        pass

    _CoreDumper.add_representer(str, _represent_core_str)
    _CORE_DUMPER = _CoreDumper
    return _CoreDumper


def dag_to_yaml(cfg: DagConfig) -> str:
    """Serialize a :class:`DagConfig` to round-trippable YAML.

    Ambiguous scalar-looking strings are quoted so the core-schema loader preserves
    them as strings. A missing declared YAML dependency raises :class:`DagJsonError`.
    """
    try:
        import yaml
    except ModuleNotFoundError as exc:
        raise DagJsonError(_MISSING_YAML_MSG) from exc
    return yaml.dump(
        _dag_to_obj(cfg),
        Dumper=_core_dumper(),
        sort_keys=False,  # preserve our deliberate key order
        allow_unicode=True,  # keep unicode raw rather than \\xNN escapes
        default_flow_style=False,  # block style, human-readable
    )
