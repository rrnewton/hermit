#!/usr/bin/env python3
"""The parity ledger store: one `parity-ledger/v1` row per parity cell of a run.

Parity is measured after determinism: hermit's post-pass compares each
candidate backend's retained verify log against the ptrace reference and writes
one `ParityRecord` per cell to `parity.jsonl`. hermit builds one
`ParityLedgerSource` per cell (`parity::ledger_sources`); this module wraps
each in the published envelope, checks every invariant the envelope and its
record promise, and hands the batch to the ledger publisher. The design is
https://github.com/rrnewton/hermit/issues/3301.

Two trees in the test ledger, both outside the `series/*/*/*.jsonl` glob
every series reader uses:

* `parity/<team>/<host>/<YYYY-MM>.jsonl` holds the rows. Their fields are
  exactly hermit's `ParityLedgerRow` (`deny_unknown_fields`), so the
  scorecard reads them with the same Rust type the post-pass writes.
* `parity-runs/<team>/<host>/<YYYY-MM>.jsonl` holds the run-level events:
  one `parity.run` marker per appended run, and one `parity.refusal` per
  append that was refused. A row type that refuses unknown fields cannot also
  carry "this run appended N rows", so the marker is its own event.

What the run-level events tell apart: a marker means the run's rows are
recorded; a refusal with no marker means `append-parity` ran and refused
them, with the refusal's message; neither means `append-parity` was never
called for the run (an older hermit, a producer that never calls it, or a run
whose parity selection was empty: hermit's callers skip `append-parity` when
nothing is in scope, so an empty selection is not told apart from an older
hermit). A marker with `records: 0` is what an empty stdin records. A node
that never ran shows up as `record-missing` rows whose origin says `absent`,
and a node whose post-pass outputs hermit refused shows up as `record-missing`
rows whose origin says `refused` and whose reason names the refusal
(`parity::expected_ledger_sources`).

A spooled batch is one file per run: the marker line, then the rows, in the
canonical compact encoding. The batch is published whole or not at all. A
refusal is a batch of its own, holding only the refusal event.

Everything here refuses loudly. A malformed record is never skipped, a
record-missing cell stays visible as a row, and credit that is null is never
turned into 0 or 1. A refused append is recorded as a refusal, never as a run
that did not happen.

Every backend is compared the same way. `inputs-not-equalized` survives only
in dbt's history, from runs before the dbt backend could apply `--bind`
(hermit's `ParityBackend::has_unequalizable_history`, mirrored in
`UNEQUALIZABLE_HISTORY_BACKENDS`), so a row of any other backend with that
verdict is refused. A run is also held to the one declaration its own rows
make.

Every record that is not `matched` or `diverged` names why in two typed
fields: `unavailable_class`, from the closed set `UNAVAILABLE_CLASSES`, and
`operand`, the side it concerns (`reference`, `candidate` or null). They are
required on every record and row, null exactly on a measured one, and must
fit the verdict and each other (`check_class`). Nothing here reads the
reason text to decide a class: `reason` is prose for a person.

Three classes name a candidate that was never run where the parity population
wanted one (hermit's `UnavailableClass::CandidateNotEnabled`,
`CandidateNotSelected` and `CandidateNotSampled`): `candidate-not-enabled`
(the manifest does not enable the candidate's verify cell),
`candidate-not-selected` (full validation enables it but does not select it)
and `candidate-not-sampled` (full validation selects it, but a sampled run
such as the pressure test did not plan it). Each goes only with verdict
`candidate-missing` and operand `candidate`, and `candidate-not-sampled` only
in a row of a sampled producer (`SAMPLED_PRODUCERS`, the pressure test): full
validation plans every candidate it enables and selects, so a validate row
of that class is refused (`check_producer_class`). Where a cell counts in a summary
is the scorecard's business (`ai_docs/parity-summary-format-v5.md`), not this
store's: the first two score 0 in the population and the third is outside it.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from decimal import Decimal
from pathlib import Path
from typing import AbstractSet, Any, Iterable, Iterator, Mapping, Sequence

SCHEMA = "parity-ledger/v1"
ROW_EVENT_TYPE = "parity.record"
RUN_EVENT_TYPE = "parity.run"
REFUSAL_EVENT_TYPE = "parity.refusal"
RETRACTION_EVENT_TYPE = "parity.retraction"
#: The same event for the series quarantine store: it withdraws published
#: `series.quarantine` records (refused inputs) that should never have been
#: published. It lives in its own tree, `QUARANTINE_RETRACTION_ROOT`, which no
#: reader of `quarantine/series/` lists, and travels through the parity spool
#: and `plan_publication` like a parity retraction. series.readmission_plan
#: and the website export drop the records it names.
QUARANTINE_RETRACTION_EVENT_TYPE = "series.quarantine-retraction"
QUARANTINE_RETRACTION_ROOT = "quarantine/retractions"
#: The events stored under `parity-runs/` rather than `parity/`.
RUN_LEVEL_EVENT_TYPES = (RUN_EVENT_TYPE, REFUSAL_EVENT_TYPE)
TEAM = "hermit"
RECORD_SCHEMA = 1
ROW_ROOT = "parity"
RUN_ROOT = "parity-runs"
#: `parity-all` is a `pressure-test.rs run --parity-all` series: every verify cell
#: of every population test on ptrace and every candidate, a disabled candidate
#: as a parity probe on ptrace's inputs. Its rows are kept apart from the
#: pressure test's own so a probe's credit is never read as a manifest cell's.
PRODUCERS = ("validate", "pressure-test", "parity-all")
#: Hermit's parity candidates (`ParityBackend` in ci/manifest-plan/src/parity.rs).
#: in-guest-trap joined them when its verify cells joined full validation.
BACKENDS = ("dbt", "in-guest-trap", "kvm", "liteinst", "sabre")
REFERENCE_BACKEND = "ptrace"
#: Most to least adverse, hermit's `LedgerVerdict` order. `nondeterministic`
#: is a real verify mismatch in an operand's history: that operand has no
#: deterministic golden log, so nothing is compared.
LEDGER_VERDICTS = (
    "record-missing",
    "nondeterministic",
    "unavailable",
    "reference-missing",
    "candidate-missing",
    "inputs-not-equalized",
    "diverged",
    "matched",
)
RECORD_VERDICTS = LEDGER_VERDICTS[1:]
MEASURED_VERDICTS = frozenset({"matched", "diverged"})
#: hermit's `LedgerPostPassState`: how far a node's post-pass got. `absent`:
#: the node left no parity.status.json; `refused`: hermit refused the node's
#: post-pass outputs. Either way the node owes only `record-missing` rows.
POST_PASS_STATES = ("complete", "failed", "running", "absent", "refused")
SCOPE_SOURCES = ("status-scope", "status-count", "expected-scope")
#: hermit's `UnavailableClass`, by its `UnavailableGroup`, each group in the
#: order the scorecard prints it and the groups in hermit's
#: `UnavailableClass::ALL` order. Which cells a summary counts where is the
#: summary format's rule (`ai_docs/parity-summary-format-v5.md`), not this
#: store's: these groups only say what kind of cause a class is. NO GOLDEN:
#: an operand's own outcome left no deterministic golden log.
NO_GOLDEN_CLASSES = (
    "determinism-mismatch",
    "timeout",
    "crash",
    "oom",
    "infrastructure-error",
    "sandbox-denied",
    "failed-untyped",
    "ended",
    "host-inapplicable",
)
#: CANDIDATE NOT RUN: full validation never runs the candidate's verify cell,
#: because the manifest does not enable it or full validation does not select
#: it. Only verdict `candidate-missing`, operand `candidate`.
CANDIDATE_NOT_RUN_CLASSES = ("candidate-not-enabled", "candidate-not-selected")
#: NOT SAMPLED: full validation enables and selects the candidate's verify
#: cell, but a sampled run (the pressure test) did not plan it. Only verdict
#: `candidate-missing`, operand `candidate`.
NOT_SAMPLED_CLASSES = ("candidate-not-sampled",)
#: The producers whose runs execute a sample of full validation, so only their
#: rows may carry a NOT SAMPLED class (`check_producer_class`).
SAMPLED_PRODUCERS = ("pressure-test",)
#: NOT COMPARED: historical only; the backend's inputs could not be equalized
#: with the reference's.
NOT_COMPARED_CLASSES = ("inputs-not-equalized",)
#: UNMEASURED: a golden log could exist but no comparison was made, a harness
#: or parity-tool defect.
UNMEASURED_CLASSES = (
    "log-not-retained",
    "log-unreadable",
    "no-result-row",
    "log-diff-failed",
    "invalid-test-id",
    "golden-not-written",
    "epoch-not-shared",
)
#: The synthesized row of a selected cell with no record.
RECORD_MISSING_CLASS = "record-missing"
#: The closed set, in hermit's `UnavailableClass::ALL` order.
UNAVAILABLE_CLASSES = (
    NO_GOLDEN_CLASSES
    + CANDIDATE_NOT_RUN_CLASSES
    + NOT_SAMPLED_CLASSES
    + NOT_COMPARED_CLASSES
    + UNMEASURED_CLASSES
    + (RECORD_MISSING_CLASS,)
)
OPERANDS = ("reference", "candidate")
#: The backends whose published history may hold `inputs-not-equalized` rows
#: (hermit's `ParityBackend::has_unequalizable_history`): dbt's, from before
#: the dbt backend could apply `--bind`. No new row has that verdict.
UNEQUALIZABLE_HISTORY_BACKENDS = frozenset({"dbt"})
#: The verdicts a class may go with (hermit's `UnavailableClass::verdicts`).
#: Any other class is an operand's failure or an unmeasured comparison, and
#: goes with `unavailable` or with the verdict that names its side
#: (`_OTHER_CLASS_VERDICTS`); `no-result-row` is one of those.
_CLASS_VERDICTS = {
    "determinism-mismatch": ("nondeterministic",),
    "inputs-not-equalized": ("inputs-not-equalized",),
    RECORD_MISSING_CLASS: ("record-missing",),
    **{name: ("candidate-missing",) for name in CANDIDATE_NOT_RUN_CLASSES + NOT_SAMPLED_CLASSES},
    "log-unreadable": ("reference-missing", "candidate-missing"),
}
_OTHER_CLASS_VERDICTS = ("unavailable", "reference-missing", "candidate-missing")
#: The operands a class may name; None is null.
_CLASS_OPERANDS: dict[str, tuple[str | None, ...]] = {
    **{name: OPERANDS for name in NO_GOLDEN_CLASSES},
    **{name: ("candidate",) for name in CANDIDATE_NOT_RUN_CLASSES + NOT_SAMPLED_CLASSES},
    "inputs-not-equalized": ("candidate",),
    "log-not-retained": OPERANDS,
    "log-unreadable": OPERANDS,
    "no-result-row": OPERANDS,
    "log-diff-failed": (None,),
    "invalid-test-id": (None,),
    "golden-not-written": ("reference",),
    "epoch-not-shared": (*OPERANDS, None),
    RECORD_MISSING_CLASS: (None,),
}
#: A verdict that names a side names the operand.
_VERDICT_OPERAND = {"reference-missing": "reference", "candidate-missing": "candidate"}
#: The largest f64 below 1.0: a divergence can never round up to a match.
BELOW_ONE = 1.0 - sys.float_info.epsilon / 2
_U64_MAX = (1 << 64) - 1
_SHA1_RE = re.compile(r"[0-9a-f]{40}")
_SHA256_RE = re.compile(r"[0-9a-f]{64}")
_EMITTED_AT_RE = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z")
# Built rather than written literally so this module does not itself contain
# the owner-path needle the portability check greps tracked files for.
_OWNER_PATH_NEEDLE = "/" + "home" + "/"

FIRST_DIFFERENCE_FIELDS = (
    "field",
    "syscall",
    "scheduler_turn",
    "virtual_nanoseconds",
    "reference_message",
    "candidate_message",
)
RECORD_FIELDS = (
    "schema",
    "test_id",
    "backend",
    "verdict",
    "unavailable_class",
    "operand",
    "inputs_equalized",
    "reason",
    "credit",
    "unequalized_credit",
    "first_divergent_record",
    "left_len",
    "right_len",
    "matched_prefix",
    "first_difference",
    "reference_log",
    "candidate_log",
    "run_id",
    "hermit_sha",
)
ORIGIN_FIELDS = (
    "lane",
    "node",
    "post_pass_state",
    "status_sha256",
    "records_sha256",
    "hermit_bin_sha256",
    "scope_source",
)
SOURCE_FIELDS = (
    "cell",
    "test_id",
    "backend",
    "verdict",
    "unavailable_class",
    "operand",
    "reason",
    "run_id",
    "hermit_sha",
    "source",
    "record",
)
ROW_FIELDS = (
    "schema",
    "event_type",
    "event_id",
    "team",
    "host",
    "emitted_at",
    "producer",
    "run_id",
    "hermit_sha",
    "source_tree_dirty",
    "cell",
    "test_id",
    "backend",
    "verdict",
    "unavailable_class",
    "operand",
    "reason",
    "source",
    "record",
)
RUN_NODE_FIELDS = ORIGIN_FIELDS + ("records",)
RUN_FIELDS = (
    "schema",
    "event_type",
    "event_id",
    "team",
    "host",
    "emitted_at",
    "producer",
    "run_id",
    "hermit_sha",
    "source_tree_dirty",
    "records",
    "verdicts",
    "nodes",
)
REFUSAL_FIELDS = (
    "schema",
    "event_type",
    "event_id",
    "team",
    "host",
    "emitted_at",
    "producer",
    "run_id",
    "hermit_sha",
    "source_tree_dirty",
    "input_sha256",
    "input_lines",
    "error",
)
#: A refusal's message is kept whole up to this many characters.
REFUSAL_ERROR_LIMIT = 4000
#: A `parity.retraction` withdraws published events that should never have
#: been published, such as test fixtures a leaking spool path let through. It
#: lives in `parity/` beside the rows it withdraws, so the scorecard's summary
#: stays a function of that one store, which its `store_sha256` covers. Nothing
#: is deleted: the retracted events stay in the ledger, and every reader drops
#: them.
RETRACTION_FIELDS = (
    "schema",
    "event_type",
    "event_id",
    "team",
    "host",
    "emitted_at",
    "retracts",
    "reason",
)
#: The fields a run marker and each of its rows must share.
SHARED_FIELDS = (
    "team",
    "host",
    "emitted_at",
    "producer",
    "run_id",
    "hermit_sha",
    "source_tree_dirty",
)


class ParityLedgerError(ValueError):
    """A parity row, marker, batch, or source that breaks the contract."""


# --------------------------------------------------------------------------- JSON


def _unique_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ParityLedgerError(f"duplicate JSON key {key!r}")
        value[key] = item
    return value


def _refuse_constant(name: str) -> Any:
    raise ParityLedgerError(f"{name} is not a JSON number")


def loads(text: str, label: str) -> Any:
    """Strict JSON: duplicate keys, NaN and Infinity are refused."""
    try:
        return json.loads(
            text, object_pairs_hook=_unique_pairs, parse_constant=_refuse_constant
        )
    except ParityLedgerError as error:
        raise ParityLedgerError(f"{label}: {error}") from error
    except (json.JSONDecodeError, RecursionError) as error:
        raise ParityLedgerError(f"{label}: not JSON: {error}") from error


def canonical_line(event: Mapping[str, Any]) -> str:
    """The one encoding a published event has: compact, fixed key order.

    The key order is the Rust struct's field order (serde writes fields in
    declaration order), so a record copied from `parity.jsonl` keeps its
    bytes.
    """
    try:
        return json.dumps(
            canonical_event(event), ensure_ascii=False, separators=(",", ":"),
            allow_nan=False,
        )
    except (TypeError, ValueError) as error:
        raise ParityLedgerError(f"cannot encode a parity event: {error}") from error


def _ordered(value: Mapping[str, Any], fields: Sequence[str]) -> dict[str, Any]:
    ordered = {field: value[field] for field in fields if field in value}
    ordered.update((key, item) for key, item in value.items() if key not in ordered)
    return ordered


def _canonical_record(record: Any) -> Any:
    if not isinstance(record, Mapping):
        return record
    ordered = _ordered(record, RECORD_FIELDS)
    if isinstance(ordered.get("first_difference"), Mapping):
        ordered["first_difference"] = _ordered(
            ordered["first_difference"], FIRST_DIFFERENCE_FIELDS
        )
    return ordered


def _owner_home_redacted(text: str) -> str:
    return re.sub(re.escape(_OWNER_PATH_NEEDLE) + r"[^/\s'\"]*", "<owner home>", text)


def _public_first_difference(source: Mapping[str, Any]) -> Mapping[str, Any]:
    """`source` with the owner home replaced in its first difference's text.

    Its field and two messages quote the guest's own log records, so a guest
    that opens a file under the checkout quotes the owner's home directory.
    Replacing it keeps the record and drops only the directory, as
    `public_error` does for a refusal's text; an owner path anywhere else is
    still refused.
    """
    record = source.get("record") if isinstance(source, Mapping) else None
    difference = record.get("first_difference") if isinstance(record, Mapping) else None
    if not isinstance(difference, Mapping):
        return source
    public = dict(difference)
    for name in ("field", "reference_message", "candidate_message"):
        if isinstance(public.get(name), str):
            public[name] = _owner_home_redacted(public[name])
    if public == difference:
        return source
    return {**source, "record": {**record, "first_difference": public}}


def canonical_event(event: Mapping[str, Any]) -> dict[str, Any]:
    """Reorder a row, marker, refusal, or source to its schema's field order."""
    if event.get("event_type") == REFUSAL_EVENT_TYPE:
        return _ordered(event, REFUSAL_FIELDS)
    if event.get("event_type") in RETRACTION_EVENT_TYPES:
        return _ordered(event, RETRACTION_FIELDS)
    if event.get("event_type") == RUN_EVENT_TYPE:
        ordered = _ordered(event, RUN_FIELDS)
        if isinstance(ordered.get("verdicts"), Mapping):
            ordered["verdicts"] = _ordered(ordered["verdicts"], LEDGER_VERDICTS)
        if isinstance(ordered.get("nodes"), list):
            ordered["nodes"] = [
                _ordered(node, RUN_NODE_FIELDS) if isinstance(node, Mapping) else node
                for node in ordered["nodes"]
            ]
        return ordered
    if "event_type" not in event and "inputs_equalized" in event:
        return _canonical_record(event)
    fields = ROW_FIELDS if "event_type" in event else SOURCE_FIELDS
    ordered = _ordered(event, fields)
    if isinstance(ordered.get("source"), Mapping):
        ordered["source"] = _ordered(ordered["source"], ORIGIN_FIELDS)
    if "record" in ordered:
        ordered["record"] = _canonical_record(ordered["record"])
    return ordered


# ------------------------------------------------------------- Rust text forms


def _rust_display(value: float) -> str:
    """Rust's `{}` for an f64: never exponent notation, no trailing `.0`."""
    if math.isnan(value):
        return "NaN"
    if math.isinf(value):
        return "inf" if value > 0 else "-inf"
    text = format(Decimal(repr(value)), "f")
    if "." in text:
        text = text.rstrip("0").rstrip(".")
    return "-0" if text in {"-0", "-0."} else text


def _rust_debug_float(value: float) -> str:
    """Rust's `{:?}` for an f64: `1.0`, `0.5`, `1e-5`."""
    if math.isnan(value):
        return "NaN"
    if math.isinf(value):
        return "inf" if value > 0 else "-inf"
    if value == 0:
        return "-0.0" if math.copysign(1.0, value) < 0 else "0.0"
    if 1e-4 <= abs(value) < 1e16:
        text = format(Decimal(repr(value)), "f")
        return text if "." in text else text + ".0"
    mantissa, _, exponent = repr(value).partition("e")
    return f"{mantissa}e{int(exponent)}"


def _rust_debug_option(value: Any) -> str:
    if value is None:
        return "None"
    if isinstance(value, float):
        return f"Some({_rust_debug_float(value)})"
    return f"Some({value})"


def _rust_debug_str(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def _camel(verdict: str) -> str:
    """Rust's `{:?}` of a `ParityVerdict`: `InputsNotEqualized`."""
    return "".join(part.capitalize() for part in verdict.split("-"))


# ------------------------------------------------------------------ primitives


def _exact_fields(value: Any, fields: Sequence[str], label: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise ParityLedgerError(f"{label} must be a JSON object, got {type(value).__name__}")
    missing = [field for field in fields if field not in value]
    unknown = sorted(set(value) - set(fields))
    if missing or unknown:
        parts = []
        if missing:
            parts.append(f"missing {', '.join(missing)}")
        if unknown:
            parts.append(f"unknown {', '.join(unknown)}")
        raise ParityLedgerError(f"{label} fields are not exactly its schema's: {'; '.join(parts)}")
    return value


def _string(value: Any, label: str) -> str:
    if not isinstance(value, str):
        raise ParityLedgerError(f"{label} must be a string, got {_kind(value)}")
    return value


def _optional_string(value: Any, label: str) -> str | None:
    return None if value is None else _string(value, label)


def _boolean(value: Any, label: str) -> bool:
    if type(value) is not bool:
        raise ParityLedgerError(f"{label} must be true or false, got {_kind(value)}")
    return value


def _count(value: Any, label: str) -> int:
    if type(value) is not int or not 0 <= value <= _U64_MAX:
        raise ParityLedgerError(f"{label} must be a nonnegative integer, got {_kind(value)}")
    return value


def _optional_count(value: Any, label: str) -> int | None:
    return None if value is None else _count(value, label)


def _optional_float(value: Any, label: str) -> float | None:
    if value is None:
        return None
    if type(value) not in (int, float):
        raise ParityLedgerError(f"{label} must be a number or null, got {_kind(value)}")
    number = float(value)
    if not math.isfinite(number):
        raise ParityLedgerError(f"{label} must be finite, got {value!r}")
    return number


def _optional_sha256(value: Any, label: str) -> str | None:
    if value is None:
        return None
    if not isinstance(value, str) or _SHA256_RE.fullmatch(value) is None:
        raise ParityLedgerError(f"{label} must be 64 lowercase hex digits or null, got {value!r}")
    return value


def _choice(value: Any, choices: Sequence[str], label: str) -> str:
    if not isinstance(value, str) or value not in choices:
        raise ParityLedgerError(
            f"{label} must be one of {', '.join(choices)}, got {value!r}"
        )
    return value


def _backend(value: Any, label: str) -> str:
    if value == REFERENCE_BACKEND:
        raise ParityLedgerError(
            f"{label}: {REFERENCE_BACKEND} is the parity reference, not a candidate backend"
        )
    if not isinstance(value, str) or value not in BACKENDS:
        shown = _rust_debug_str(value) if isinstance(value, str) else repr(value)
        raise ParityLedgerError(
            f"{label}: unknown parity backend {shown}; expected one of {', '.join(BACKENDS)}"
        )
    return value


def _segment(value: Any, label: str) -> str:
    text = _string(value, label)
    if not text or "/" in text or text in {".", ".."} or "\0" in text or "__" in text:
        raise ParityLedgerError(f"{label} must be a single path segment, got {text!r}")
    return text


def _kind(value: Any) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "a boolean"
    if isinstance(value, (int, float)):
        return f"the number {value!r}"
    if isinstance(value, str):
        return f"the string {value!r}"
    return f"a {type(value).__name__}"


def _sha256_hex(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def contains_owner_path(value: Any) -> bool:
    if isinstance(value, str):
        return _OWNER_PATH_NEEDLE in value
    if isinstance(value, Mapping):
        return any(contains_owner_path(item) for item in value.values())
    if isinstance(value, (list, tuple)):
        return any(contains_owner_path(item) for item in value)
    return False


# ---------------------------------------------------------------------- credit


def credit(matched_prefix: int, left_len: int, right_len: int) -> float | None:
    """hermit's `parity::credit`: the matched prefix over the longer stream.

    Full credit only when every message on both sides matched. A divergence
    never rounds up to 1.0. None when nothing was compared: unmeasured is
    never 0.
    """
    longer = max(left_len, right_len)
    if longer == 0:
        return None
    matched = min(matched_prefix, min(left_len, right_len))
    if matched == longer:
        return 1.0
    return min(max(matched / longer, 0.0), BELOW_ONE)


def _shown(value: str | None) -> str:
    return "null" if value is None else value


def check_class(
    at: str, backend: str, verdict: str, unavailable_class: Any, operand: Any
) -> None:
    """The typed reason a record, row or summary cell gives for its verdict.

    `unavailable_class` is null exactly on a measured (matched or diverged)
    verdict. Otherwise it is one class of the closed set, it fits the verdict
    (`determinism-mismatch` goes with exactly `nondeterministic`, and so on),
    and `operand` is the side the class concerns (`_CLASS_OPERANDS`), which a
    `reference-missing` or `candidate-missing` verdict also names. Only a
    backend in `UNEQUALIZABLE_HISTORY_BACKENDS` may have the historical
    `inputs-not-equalized` verdict; otherwise every backend is held to the
    same rules. The reason text is never read.
    """
    if unavailable_class is not None:
        _choice(unavailable_class, UNAVAILABLE_CLASSES, f"{at}: unavailable_class")
    if operand is not None:
        _choice(operand, OPERANDS, f"{at}: operand")
    if verdict == "inputs-not-equalized" and backend not in UNEQUALIZABLE_HISTORY_BACKENDS:
        raise ParityLedgerError(
            f"{at}: {backend} inputs can be equalized; a comparison with unequal inputs is "
            "measured and reports unequalized_credit"
        )
    if verdict in MEASURED_VERDICTS:
        if unavailable_class is not None:
            raise ParityLedgerError(
                f"{at}: a {verdict} verdict is measured and carries no unavailable_class, "
                f"got {unavailable_class}"
            )
        if operand is not None:
            raise ParityLedgerError(
                f"{at}: a {verdict} verdict is measured and names no operand, got {operand}"
            )
        return
    if unavailable_class is None:
        raise ParityLedgerError(f"{at}: verdict {verdict} needs an unavailable_class")
    verdicts = _CLASS_VERDICTS.get(unavailable_class, _OTHER_CLASS_VERDICTS)
    if verdict not in verdicts:
        raise ParityLedgerError(
            f"{at}: unavailable_class {unavailable_class} cannot go with verdict {verdict}; "
            f"it needs {' or '.join(verdicts)}"
        )
    operands = _CLASS_OPERANDS[unavailable_class]
    if operand not in operands:
        raise ParityLedgerError(
            f"{at}: unavailable_class {unavailable_class} needs operand "
            f"{' or '.join(_shown(side) for side in operands)}, got {_shown(operand)}"
        )
    side = _VERDICT_OPERAND.get(verdict)
    if side is not None and operand != side:
        raise ParityLedgerError(f"{at}: verdict {verdict} names the {side} operand, got {_shown(operand)}")


def check_producer_class(at: str, producer: str, unavailable_class: Any) -> None:
    """A NOT SAMPLED class names a candidate a sampled run did not plan, so
    only a row of a producer in `SAMPLED_PRODUCERS` may carry it. A validate
    run plans every candidate full validation enables and selects; a cell of
    its that has no row is `no-result-row` or `record-missing`, which score 0,
    never a cell outside the population."""
    if unavailable_class in NOT_SAMPLED_CLASSES and producer not in SAMPLED_PRODUCERS:
        raise ParityLedgerError(
            f"{at}: unavailable_class {unavailable_class} names a candidate a sampled run did not plan; "
            f"only a {' or '.join(SAMPLED_PRODUCERS)} row may carry it, not a {producer} row"
        )


def measured_credit(record: Mapping[str, Any]) -> float | None:
    """Clean credit if set, else the unequalized measurement, else None."""
    value = record.get("credit")
    if value is None:
        value = record.get("unequalized_credit")
    return None if value is None else float(value)


# --------------------------------------------------------------------- records


def validate_record(record: Any) -> None:
    """hermit's `ParityRecord` shape and `ParityRecord::validate`."""
    value = _exact_fields(record, RECORD_FIELDS, "parity record")
    test_id = _string(value["test_id"], "parity record test_id")
    backend = _backend(value["backend"], f"parity record {test_id}")
    at = f"parity record {test_id}@{backend}"
    schema = _count(value["schema"], f"{at}: schema")
    verdict = _choice(value["verdict"], RECORD_VERDICTS, f"{at}: verdict")
    unavailable_class = _optional_string(value["unavailable_class"], f"{at}: unavailable_class")
    operand = _optional_string(value["operand"], f"{at}: operand")
    inputs_equalized = _boolean(value["inputs_equalized"], f"{at}: inputs_equalized")
    reason = _optional_string(value["reason"], f"{at}: reason")
    clean = _optional_float(value["credit"], f"{at}: credit")
    unequalized = _optional_float(value["unequalized_credit"], f"{at}: unequalized_credit")
    first_record = _optional_count(value["first_divergent_record"], f"{at}: first_divergent_record")
    left = _optional_count(value["left_len"], f"{at}: left_len")
    right = _optional_count(value["right_len"], f"{at}: right_len")
    prefix = _optional_count(value["matched_prefix"], f"{at}: matched_prefix")
    difference = value["first_difference"]
    if difference is not None:
        fields = _exact_fields(difference, FIRST_DIFFERENCE_FIELDS, f"{at}: first_difference")
        _optional_string(fields["field"], f"{at}: first_difference.field")
        for name in ("syscall", "scheduler_turn", "virtual_nanoseconds"):
            _optional_count(fields[name], f"{at}: first_difference.{name}")
        for name in ("reference_message", "candidate_message"):
            _optional_string(fields[name], f"{at}: first_difference.{name}")
    reference_log = _optional_string(value["reference_log"], f"{at}: reference_log")
    candidate_log = _optional_string(value["candidate_log"], f"{at}: candidate_log")
    run_id = _string(value["run_id"], f"{at}: run_id")
    hermit_sha = _string(value["hermit_sha"], f"{at}: hermit_sha")

    if schema != RECORD_SCHEMA:
        raise ParityLedgerError(f"{at}: schema must be {RECORD_SCHEMA}, got {schema}")
    if not test_id.strip() or not run_id.strip():
        raise ParityLedgerError(f"{at}: test_id and run_id must be non-empty")
    if _SHA1_RE.fullmatch(hermit_sha) is None:
        raise ParityLedgerError(
            f"{at}: hermit_sha must be 40 lowercase hex digits, got {_rust_debug_str(hermit_sha)}"
        )
    for amount in (clean, unequalized):
        if amount is not None and not 0.0 <= amount <= 1.0:
            raise ParityLedgerError(f"{at}: credit {_rust_display(amount)} is outside [0, 1]")
    if clean is not None and unequalized is not None:
        raise ParityLedgerError(f"{at}: credit and unequalized_credit are exclusive")
    if clean is not None and not inputs_equalized:
        raise ParityLedgerError(
            f"{at}: credit is clean credit and needs equalized inputs; this measurement "
            "belongs in unequalized_credit"
        )
    if unequalized is not None and inputs_equalized:
        raise ParityLedgerError(f"{at}: unequalized_credit is set but the inputs were equalized")
    measured = verdict in MEASURED_VERDICTS
    # A historical not-compared record never claims equal inputs.
    if verdict == "inputs-not-equalized" and inputs_equalized:
        raise ParityLedgerError(
            f"{at}: an inputs-not-equalized record was not compared and cannot claim equal inputs"
        )
    check_class(at, backend, verdict, unavailable_class, operand)
    amount = clean if clean is not None else unequalized
    if measured:
        if reason is not None:
            raise ParityLedgerError(f"{at}: a measured verdict carries no reason")
        if left is None or right is None:
            raise ParityLedgerError(f"{at}: a measured verdict needs both lengths")
        if reference_log is None or candidate_log is None:
            raise ParityLedgerError(f"{at}: a measured verdict needs both log paths")
        if prefix is not None and prefix > min(left, right):
            raise ParityLedgerError(
                f"{at}: matched prefix {prefix} exceeds the shorter compared stream of "
                f"{left} | {right}"
            )
        if verdict == "matched" and (first_record is not None or difference is not None):
            raise ParityLedgerError(f"{at}: a match carries no divergence position")
        expected = None if prefix is None else credit(prefix, left, right)
        if amount != expected:
            raise ParityLedgerError(
                f"{at}: credit {_rust_debug_option(amount)} disagrees with matched prefix "
                f"{_rust_debug_option(prefix)} of {left} | {right}"
            )
        if verdict == "matched" and amount != 1.0:
            raise ParityLedgerError(
                f"{at}: a match must be full credit, got {_rust_debug_option(amount)}"
            )
        if verdict == "diverged" and amount == 1.0:
            raise ParityLedgerError(f"{at}: a divergence cannot be full credit")
        return
    name = _camel(verdict)
    if reason is None or not reason.strip():
        raise ParityLedgerError(f"{at}: {name} needs a reason")
    if amount is not None or prefix is not None or first_record is not None or difference is not None:
        raise ParityLedgerError(f"{at}: {name} is unmeasured and carries no credit or divergence")
    if left is not None or right is not None:
        raise ParityLedgerError(f"{at}: {name} is unmeasured and carries no compared lengths")
    if (verdict == "reference-missing" and reference_log is not None) or (
        verdict == "candidate-missing" and candidate_log is not None
    ):
        raise ParityLedgerError(f"{at}: {name} names the log it says is missing")


# -------------------------------------------------------------- source and row


def validate_origin(origin: Any, at: str) -> Mapping[str, Any]:
    value = _exact_fields(origin, ORIGIN_FIELDS, f"{at}: source")
    _string(value["lane"], f"{at}: source.lane")
    _string(value["node"], f"{at}: source.node")
    _choice(value["post_pass_state"], POST_PASS_STATES, f"{at}: source.post_pass_state")
    for name in ("status_sha256", "records_sha256", "hermit_bin_sha256"):
        _optional_sha256(value[name], f"{at}: source.{name}")
    _choice(value["scope_source"], SCOPE_SOURCES, f"{at}: source.scope_source")
    return value


def _check_ledger_fields(
    at: str,
    cell: str,
    test_id: str,
    backend: str,
    verdict: str,
    unavailable_class: str | None,
    operand: str | None,
    reason: str | None,
    run_id: str | None,
    hermit_sha: str | None,
    origin: Mapping[str, Any],
    record: Any,
) -> None:
    """hermit's `check_ledger_fields`, check for check and message for message."""
    if cell != f"{test_id}@{backend}":
        raise ParityLedgerError(f"{at}: cell {_rust_debug_str(cell)} is not {test_id}@{backend}")
    if not origin["lane"] or not origin["node"]:
        raise ParityLedgerError(f"{at}: source lane and node must be non-empty")
    if verdict not in MEASURED_VERDICTS and (reason is None or not reason.strip()):
        raise ParityLedgerError(f"{at}: verdict {verdict} needs a reason")
    check_class(at, backend, verdict, unavailable_class, operand)
    if origin["post_pass_state"] == "absent" and verdict != "record-missing":
        raise ParityLedgerError(
            f"{at}: a node that left no status can only owe record-missing rows, got {verdict}"
        )
    if origin["post_pass_state"] == "refused" and verdict != "record-missing":
        raise ParityLedgerError(
            f"{at}: a node whose outputs were refused can only owe record-missing rows, got {verdict}"
        )
    if verdict == "record-missing":
        if record is not None:
            raise ParityLedgerError(f"{at}: a record-missing row carries no record")
        return
    if record is None:
        raise ParityLedgerError(f"{at}: verdict {verdict} needs its record")
    try:
        validate_record(record)
    except ParityLedgerError as error:
        raise ParityLedgerError(f"{at}: {error}") from error
    if record["verdict"] != verdict:
        raise ParityLedgerError(
            f"{at}: row verdict {verdict} disagrees with record verdict {record['verdict']}"
        )
    if record["test_id"] != test_id or record["backend"] != backend:
        raise ParityLedgerError(f"{at}: record names {record['test_id']}@{record['backend']}")
    if record["unavailable_class"] != unavailable_class:
        raise ParityLedgerError(
            f"{at}: row unavailable_class {_shown(unavailable_class)} disagrees with the "
            f"record's {_shown(record['unavailable_class'])}"
        )
    if record["operand"] != operand:
        raise ParityLedgerError(
            f"{at}: row operand {_shown(operand)} disagrees with the record's {_shown(record['operand'])}"
        )
    if record["reason"] != reason:
        raise ParityLedgerError(f"{at}: row reason disagrees with the record's")
    if run_id is not None and run_id != record["run_id"]:
        raise ParityLedgerError(
            f"{at}: row run_id {_rust_debug_str(run_id)} disagrees with record run_id "
            f"{_rust_debug_str(record['run_id'])}"
        )
    if hermit_sha is not None and hermit_sha != record["hermit_sha"]:
        raise ParityLedgerError(
            f"{at}: row hermit_sha {_rust_debug_str(hermit_sha)} disagrees with record "
            f"hermit_sha {_rust_debug_str(record['hermit_sha'])}"
        )


def validate_source(source: Any) -> Mapping[str, Any]:
    """One `ParityLedgerSource` as hermit sends it on stdin."""
    value = _exact_fields(source, SOURCE_FIELDS, "parity ledger source")
    cell = _string(value["cell"], "parity ledger source cell")
    at = f"parity ledger source {cell}"
    test_id = _string(value["test_id"], f"{at}: test_id")
    backend = _backend(value["backend"], at)
    verdict = _choice(value["verdict"], LEDGER_VERDICTS, f"{at}: verdict")
    unavailable_class = _optional_string(value["unavailable_class"], f"{at}: unavailable_class")
    operand = _optional_string(value["operand"], f"{at}: operand")
    reason = _optional_string(value["reason"], f"{at}: reason")
    run_id = _optional_string(value["run_id"], f"{at}: run_id")
    hermit_sha = _optional_string(value["hermit_sha"], f"{at}: hermit_sha")
    origin = validate_origin(value["source"], at)
    _check_ledger_fields(
        at, cell, test_id, backend, verdict, unavailable_class, operand, reason, run_id, hermit_sha,
        origin, value["record"],
    )
    return value


def event_id(producer: str, run_id: str, cell: str, lane: str, node: str) -> str:
    """`sha256(producer \\0 run_id \\0 cell \\0 lane \\0 node)`, hermit's
    `parity_event_id`."""
    return _sha256_hex(f"{producer}\0{run_id}\0{cell}\0{lane}\0{node}")


def run_event_id(producer: str, run_id: str) -> str:
    """The run marker's identity; one marker per (producer, run)."""
    return _sha256_hex(f"{RUN_EVENT_TYPE}\0{producer}\0{run_id}")


def _envelope_fields(value: Mapping[str, Any], at: str) -> None:
    _segment(value["team"], f"{at}: team")
    _segment(value["host"], f"{at}: host")
    emitted_at = _string(value["emitted_at"], f"{at}: emitted_at")
    if _EMITTED_AT_RE.fullmatch(emitted_at) is None:
        raise ParityLedgerError(f"{at}: emitted_at must be YYYY-MM-DDTHH:MM:SSZ, got {emitted_at!r}")
    _choice(value["producer"], PRODUCERS, f"{at}: producer")
    _boolean(value["source_tree_dirty"], f"{at}: source_tree_dirty")


def validate_row(row: Any) -> Mapping[str, Any]:
    """One published `ParityLedgerRow` and `ParityLedgerRow::validate`."""
    value = _exact_fields(row, ROW_FIELDS, "parity ledger row")
    cell = _string(value["cell"], "parity ledger row cell")
    producer = _string(value["producer"], f"parity ledger row {cell}: producer")
    run_id = _string(value["run_id"], f"parity ledger row {cell}: run_id")
    at = f"parity ledger row {cell} ({producer} run {run_id})"
    schema = _string(value["schema"], f"{at}: schema")
    event_type = _string(value["event_type"], f"{at}: event_type")
    if schema != SCHEMA:
        raise ParityLedgerError(f"{at}: schema must be {SCHEMA}, got {_rust_debug_str(schema)}")
    if event_type != ROW_EVENT_TYPE:
        raise ParityLedgerError(
            f"{at}: event_type must be {ROW_EVENT_TYPE}, got {_rust_debug_str(event_type)}"
        )
    _envelope_fields(value, at)
    if not run_id.strip():
        raise ParityLedgerError(f"{at}: run_id must be non-empty")
    hermit_sha = _string(value["hermit_sha"], f"{at}: hermit_sha")
    if _SHA1_RE.fullmatch(hermit_sha) is None:
        raise ParityLedgerError(
            f"{at}: hermit_sha must be 40 lowercase hex digits, got {_rust_debug_str(hermit_sha)}"
        )
    test_id = _string(value["test_id"], f"{at}: test_id")
    backend = _backend(value["backend"], at)
    verdict = _choice(value["verdict"], LEDGER_VERDICTS, f"{at}: verdict")
    unavailable_class = _optional_string(value["unavailable_class"], f"{at}: unavailable_class")
    operand = _optional_string(value["operand"], f"{at}: operand")
    reason = _optional_string(value["reason"], f"{at}: reason")
    origin = validate_origin(value["source"], at)
    stated = _string(value["event_id"], f"{at}: event_id")
    expected = event_id(producer, run_id, cell, origin["lane"], origin["node"])
    if stated != expected:
        raise ParityLedgerError(
            f"{at}: event_id {stated} is not sha256(producer, run_id, cell, lane, node) = {expected}"
        )
    _check_ledger_fields(
        at, cell, test_id, backend, verdict, unavailable_class, operand, reason, run_id, hermit_sha,
        origin, value["record"],
    )
    check_producer_class(at, producer, unavailable_class)
    return value


def _node_key(origin: Mapping[str, Any]) -> tuple[str, str]:
    return str(origin["lane"]), str(origin["node"])


def _verdict_counts(rows: Iterable[Mapping[str, Any]]) -> dict[str, int]:
    counts = {verdict: 0 for verdict in LEDGER_VERDICTS}
    for row in rows:
        counts[row["verdict"]] += 1
    return counts


def _run_nodes(rows: Sequence[Mapping[str, Any]]) -> list[dict[str, Any]]:
    """One entry per node: its origin (identical on every row) and row count."""
    nodes: dict[tuple[str, str], dict[str, Any]] = {}
    for row in rows:
        origin = row["source"]
        key = _node_key(origin)
        known = nodes.get(key)
        if known is None:
            nodes[key] = {**{field: origin[field] for field in ORIGIN_FIELDS}, "records": 1}
            continue
        if any(known[field] != origin[field] for field in ORIGIN_FIELDS):
            raise ParityLedgerError(
                f"parity rows for node {key[0]}/{key[1]} disagree about the node's origin"
            )
        known["records"] += 1
    return [nodes[key] for key in sorted(nodes)]


def validate_run(marker: Any) -> Mapping[str, Any]:
    """One `parity.run` marker: what one run appended, and from which nodes."""
    value = _exact_fields(marker, RUN_FIELDS, "parity run marker")
    producer = _string(value["producer"], "parity run marker producer")
    run_id = _string(value["run_id"], "parity run marker run_id")
    at = f"parity run marker ({producer} run {run_id})"
    if value["schema"] != SCHEMA:
        raise ParityLedgerError(f"{at}: schema must be {SCHEMA}, got {value['schema']!r}")
    if value["event_type"] != RUN_EVENT_TYPE:
        raise ParityLedgerError(
            f"{at}: event_type must be {RUN_EVENT_TYPE}, got {value['event_type']!r}"
        )
    _envelope_fields(value, at)
    if not run_id.strip():
        raise ParityLedgerError(f"{at}: run_id must be non-empty")
    hermit_sha = _string(value["hermit_sha"], f"{at}: hermit_sha")
    if _SHA1_RE.fullmatch(hermit_sha) is None:
        raise ParityLedgerError(f"{at}: hermit_sha must be 40 lowercase hex digits, got {hermit_sha!r}")
    expected = run_event_id(producer, run_id)
    if value["event_id"] != expected:
        raise ParityLedgerError(
            f"{at}: event_id {value['event_id']!r} is not sha256(parity.run, producer, run_id) = {expected}"
        )
    records = _count(value["records"], f"{at}: records")
    verdicts = _exact_fields(value["verdicts"], LEDGER_VERDICTS, f"{at}: verdicts")
    if sum(_count(verdicts[name], f"{at}: verdicts.{name}") for name in LEDGER_VERDICTS) != records:
        raise ParityLedgerError(f"{at}: verdict counts do not sum to its {records} record(s)")
    nodes = value["nodes"]
    if not isinstance(nodes, list):
        raise ParityLedgerError(f"{at}: nodes must be a list")
    keys = []
    for index, node in enumerate(nodes):
        entry = _exact_fields(node, RUN_NODE_FIELDS, f"{at}: nodes[{index}]")
        validate_origin({field: entry[field] for field in ORIGIN_FIELDS}, f"{at}: nodes[{index}]")
        if not entry["lane"] or not entry["node"]:
            raise ParityLedgerError(f"{at}: nodes[{index}] lane and node must be non-empty")
        if _count(entry["records"], f"{at}: nodes[{index}].records") == 0:
            raise ParityLedgerError(f"{at}: nodes[{index}] names a node with no rows")
        keys.append(_node_key(entry))
    if keys != sorted(set(keys)):
        raise ParityLedgerError(f"{at}: nodes must be unique and sorted by lane and node")
    if sum(node["records"] for node in nodes) != records:
        raise ParityLedgerError(f"{at}: node record counts do not sum to its {records} record(s)")
    return value


def refusal_event_id(producer: str, run_id: str, input_sha256: str, error: str) -> str:
    """A refusal's identity: one input refused for one reason is one event."""
    return _sha256_hex(f"{REFUSAL_EVENT_TYPE}\0{producer}\0{run_id}\0{input_sha256}\0{error}")


_TRUNCATED = " ... (truncated)"


def public_error(text: Any) -> str:
    """A refusal message fit for the public ledger: no owner path, bounded.

    An owner home directory is replaced by `<owner home>`; a message longer
    than `REFUSAL_ERROR_LIMIT` keeps its start and says it was truncated.
    """
    redacted = _owner_home_redacted(str(text)).strip()
    if not redacted:
        redacted = "refused with no message"
    if len(redacted) > REFUSAL_ERROR_LIMIT:
        redacted = redacted[:REFUSAL_ERROR_LIMIT - len(_TRUNCATED)] + _TRUNCATED
    return redacted


def validate_refusal(event: Any) -> Mapping[str, Any]:
    """One `parity.refusal`: an `append-parity` call that recorded no rows, and why."""
    value = _exact_fields(event, REFUSAL_FIELDS, "parity refusal")
    producer = _string(value["producer"], "parity refusal producer")
    run_id = _string(value["run_id"], "parity refusal run_id")
    at = f"parity refusal ({producer} run {run_id})"
    if value["schema"] != SCHEMA:
        raise ParityLedgerError(f"{at}: schema must be {SCHEMA}, got {value['schema']!r}")
    if value["event_type"] != REFUSAL_EVENT_TYPE:
        raise ParityLedgerError(
            f"{at}: event_type must be {REFUSAL_EVENT_TYPE}, got {value['event_type']!r}"
        )
    _segment(value["team"], f"{at}: team")
    _segment(value["host"], f"{at}: host")
    emitted_at = _string(value["emitted_at"], f"{at}: emitted_at")
    if _EMITTED_AT_RE.fullmatch(emitted_at) is None:
        raise ParityLedgerError(f"{at}: emitted_at must be YYYY-MM-DDTHH:MM:SSZ, got {emitted_at!r}")
    _choice(producer, PRODUCERS, f"{at}: producer")
    if not run_id.strip() or "\0" in run_id:
        raise ParityLedgerError(f"{at}: run_id must be non-empty")
    hermit_sha = _string(value["hermit_sha"], f"{at}: hermit_sha")
    if _SHA1_RE.fullmatch(hermit_sha) is None:
        raise ParityLedgerError(f"{at}: hermit_sha must be 40 lowercase hex digits, got {hermit_sha!r}")
    # Null when the refusal is that the caller never said whether its tree was dirty.
    if value["source_tree_dirty"] is not None:
        _boolean(value["source_tree_dirty"], f"{at}: source_tree_dirty")
    input_sha256 = _string(value["input_sha256"], f"{at}: input_sha256")
    if _SHA256_RE.fullmatch(input_sha256) is None:
        raise ParityLedgerError(f"{at}: input_sha256 must be 64 lowercase hex digits")
    _optional_count(value["input_lines"], f"{at}: input_lines")
    error = _string(value["error"], f"{at}: error")
    if error != public_error(error):
        raise ParityLedgerError(
            f"{at}: error must be non-empty, trimmed, free of owner paths and at most "
            f"{REFUSAL_ERROR_LIMIT} characters"
        )
    if contains_owner_path(value):
        raise ParityLedgerError(f"{at}: contains a workspace-local owner path; the parity ledger is public")
    expected = refusal_event_id(producer, run_id, input_sha256, error)
    if value["event_id"] != expected:
        raise ParityLedgerError(
            f"{at}: event_id {value['event_id']!r} is not sha256(parity.refusal, producer, run_id, "
            f"input_sha256, error) = {expected}"
        )
    return value


#: A retraction's text is checked by rules a byte-level reader can apply
#: identically (hermit's scorecard is one): ASCII only, so no Unicode digit,
#: whitespace or surrogate rule can make two readers disagree, and no NUL, so
#: the NUL-joined identity below is injective (format v4 section 9).
_RETRACTION_SEGMENT_RE = re.compile(r"[A-Za-z0-9._-]+")
_RETRACTION_EMITTED_AT_RE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z")
_RETRACTION_REASON_RE = re.compile(r"[\x20-\x7e]+")
#: The ids each kind of retraction may name: parity event ids, or series
#: quarantine event ids (series._QUARANTINE_ID_RE).
_RETRACTION_TARGETS = {
    RETRACTION_EVENT_TYPE: (_SHA256_RE, "64-hex event ids"),
    QUARANTINE_RETRACTION_EVENT_TYPE: (re.compile(r"series-quarantine-[0-9a-f]{64}"),
                                       "series-quarantine-<64 hex> event ids"),
}
RETRACTION_EVENT_TYPES = tuple(_RETRACTION_TARGETS)


def retraction_event_id(
    retracts: Sequence[str], reason: str, event_type: str = RETRACTION_EVENT_TYPE,
) -> str:
    """A retraction's identity: the same events withdrawn for the same reason."""
    return _sha256_hex("\0".join((event_type, *retracts, reason)))


def validate_retraction(event: Any) -> Mapping[str, Any]:
    """One `parity.retraction`: published events every reader must drop, and why.

    `retracts` names at least one event id, each once, in ascending order, so
    one set of events withdrawn for one reason has exactly one encoding and
    one `event_id`. Whether each named event is published is checked when the
    retraction is published (`plan_publication`) and again by each reader.
    """
    value = _exact_fields(event, RETRACTION_FIELDS, "parity retraction")
    at = "parity retraction"
    if value["schema"] != SCHEMA:
        raise ParityLedgerError(f"{at}: schema must be {SCHEMA}, got {value['schema']!r}")
    if value["event_type"] not in _RETRACTION_TARGETS:
        raise ParityLedgerError(
            f"{at}: event_type must be one of {RETRACTION_EVENT_TYPES}, got {value['event_type']!r}"
        )
    target, target_text = _RETRACTION_TARGETS[value["event_type"]]
    for field in ("team", "host"):
        segment = _string(value[field], f"{at}: {field}")
        if (_RETRACTION_SEGMENT_RE.fullmatch(segment) is None or segment in {".", ".."}
                or "__" in segment):
            raise ParityLedgerError(
                f"{at}: {field} must be one path segment of ASCII letters, digits, '.', '_' and '-', "
                f"got {segment!r}"
            )
    emitted_at = _string(value["emitted_at"], f"{at}: emitted_at")
    if _RETRACTION_EMITTED_AT_RE.fullmatch(emitted_at) is None:
        raise ParityLedgerError(
            f"{at}: emitted_at must be YYYY-MM-DDTHH:MM:SSZ in ASCII digits, got {emitted_at!r}"
        )
    retracts = value["retracts"]
    if (not isinstance(retracts, list) or not retracts
            or not all(isinstance(item, str) and target.fullmatch(item) for item in retracts)):
        raise ParityLedgerError(f"{at}: retracts must be a non-empty list of {target_text}")
    if retracts != sorted(set(retracts)):
        raise ParityLedgerError(f"{at}: retracts must name each event id once, in ascending order")
    reason = _string(value["reason"], f"{at}: reason")
    if (_RETRACTION_REASON_RE.fullmatch(reason) is None or reason[0] == " " or reason[-1] == " "
            or len(reason) > REFUSAL_ERROR_LIMIT or _OWNER_PATH_NEEDLE in reason):
        raise ParityLedgerError(
            f"{at}: reason must be printable ASCII without leading or trailing spaces, free of owner "
            f"paths and at most {REFUSAL_ERROR_LIMIT} characters"
        )
    expected = retraction_event_id(retracts, reason, value["event_type"])
    if value["event_id"] != expected:
        raise ParityLedgerError(
            f"{at}: event_id {value['event_id']!r} is not sha256({value['event_type']}, retracts..., "
            f"reason) = {expected}"
        )
    return value


def build_retraction(
    retracts: Iterable[str], reason: str, *, team: str, host: str, emitted_at: str,
    event_type: str = RETRACTION_EVENT_TYPE,
) -> dict[str, Any]:
    """The retraction of kind `event_type` withdrawing `retracts` for `reason`, validated."""
    ordered = sorted(set(retracts))
    event = canonical_event({
        "schema": SCHEMA,
        "event_type": event_type,
        "event_id": retraction_event_id(ordered, reason, event_type),
        "team": team,
        "host": host,
        "emitted_at": emitted_at,
        "retracts": ordered,
        "reason": reason,
    })
    validate_retraction(event)
    return event


def retracted_events(
    events: Iterable[Mapping[str, Any]], event_type: str = RETRACTION_EVENT_TYPE,
) -> dict[str, str]:
    """Every event id a well-formed retraction of kind `event_type` among
    `events` names, mapped to the retraction's event id. A malformed
    retraction withdraws nothing; the reader that met it reports it like any
    other malformed line.
    """
    retracted: dict[str, str] = {}
    for event in events:
        if event.get("event_type") != event_type:
            continue
        try:
            validate_retraction(event)
        except ParityLedgerError:
            continue
        for item in event["retracts"]:
            retracted.setdefault(item, event["event_id"])
    return retracted


def validate_event(event: Any) -> Mapping[str, Any]:
    """One published event of either tree: a row, a run marker, a refusal, or a retraction."""
    kind = event.get("event_type") if isinstance(event, Mapping) else None
    if kind == RUN_EVENT_TYPE:
        return validate_run(event)
    if kind == REFUSAL_EVENT_TYPE:
        return validate_refusal(event)
    if kind in RETRACTION_EVENT_TYPES:
        return validate_retraction(event)
    return validate_row(event)


def check_declared_measurability(rows: Sequence[Mapping[str, Any]], label: str) -> dict[str, str]:
    """Hold one run's rows to one declaration per backend.

    A run whose hermit could not equalize a backend's inputs (dbt, before it
    could apply `--bind`) wrote an `inputs-not-equalized` record, with that
    declaration as its reason, for the backend's cells. `check_class` refuses
    that verdict for any backend outside `UNEQUALIZABLE_HISTORY_BACKENDS`, and
    `validate_record` refuses one that claims equal inputs; what is left for a
    run is that its hermit makes one declaration per backend. Returns backend
    -> declaration.
    """
    declared: dict[str, str] = {}
    for index, row in enumerate(rows, 2):
        if row["verdict"] != "inputs-not-equalized":
            continue
        backend, why = row["backend"], row["reason"]
        known = declared.setdefault(backend, why)
        if known != why:
            raise ParityLedgerError(
                f"{label}: event {index} ({row['cell']}) declares {backend} inputs not "
                f"equalizable for {why!r}, but an earlier row of the run declares {known!r}; "
                "a run's hermit makes one declaration per backend"
            )
    return declared


def validate_batch(events: Sequence[Any], label: str = "parity batch") -> tuple[dict, list[dict]]:
    """One run's batch: its marker first, then every row of that run.

    Returns the marker and the rows. The marker's counts must be the rows'
    counts, every row must share the marker's run fields, and a node's rows
    must agree about the node's origin.
    """
    if not events:
        raise ParityLedgerError(f"{label}: a batch holds at least its run marker")
    marker = events[0]
    if isinstance(marker, Mapping) and marker.get("event_type") == REFUSAL_EVENT_TYPE:
        # A refused append records only why it was refused.
        if len(events) != 1:
            raise ParityLedgerError(f"{label}: a refusal batch holds only its refusal event")
        try:
            validate_refusal(marker)
        except ParityLedgerError as error:
            raise ParityLedgerError(f"{label}: {error}") from error
        return dict(marker), []
    if isinstance(marker, Mapping) and marker.get("event_type") in RETRACTION_EVENT_TYPES:
        if len(events) != 1:
            raise ParityLedgerError(f"{label}: a retraction batch holds only its retraction event")
        try:
            validate_retraction(marker)
        except ParityLedgerError as error:
            raise ParityLedgerError(f"{label}: {error}") from error
        return dict(marker), []
    if not isinstance(marker, Mapping) or marker.get("event_type") != RUN_EVENT_TYPE:
        raise ParityLedgerError(f"{label}: the first event must be the {RUN_EVENT_TYPE} marker")
    try:
        validate_run(marker)
    except ParityLedgerError as error:
        raise ParityLedgerError(f"{label}: {error}") from error
    rows = list(events[1:])
    seen: set[str] = set()
    for index, row in enumerate(rows, 2):
        if isinstance(row, Mapping) and row.get("event_type") == RUN_EVENT_TYPE:
            raise ParityLedgerError(f"{label}: event {index} is a second run marker")
        if isinstance(row, Mapping) and row.get("event_type") == REFUSAL_EVENT_TYPE:
            raise ParityLedgerError(f"{label}: event {index} is a refusal inside a recorded run")
        if isinstance(row, Mapping) and row.get("event_type") in RETRACTION_EVENT_TYPES:
            raise ParityLedgerError(f"{label}: event {index} is a retraction inside a recorded run")
        try:
            validate_row(row)
        except ParityLedgerError as error:
            raise ParityLedgerError(f"{label}: event {index}: {error}") from error
        for field in SHARED_FIELDS:
            if row[field] != marker[field]:
                raise ParityLedgerError(
                    f"{label}: event {index} ({row['cell']}) has {field} {row[field]!r} but its "
                    f"run marker has {marker[field]!r}"
                )
        if row["event_id"] in seen:
            raise ParityLedgerError(
                f"{label}: event {index} repeats {row['cell']} from node "
                f"{row['source']['lane']}/{row['source']['node']}"
            )
        seen.add(row["event_id"])
    check_declared_measurability(rows, label)
    try:
        nodes = _run_nodes(rows)
    except ParityLedgerError as error:
        raise ParityLedgerError(f"{label}: {error}") from error
    if marker["records"] != len(rows):
        raise ParityLedgerError(
            f"{label}: run marker says {marker['records']} record(s) but the batch holds {len(rows)}"
        )
    counts = _verdict_counts(rows)
    if dict(marker["verdicts"]) != counts:
        raise ParityLedgerError(f"{label}: run marker verdict counts disagree with its rows")
    if [dict(node) for node in marker["nodes"]] != nodes:
        raise ParityLedgerError(f"{label}: run marker nodes disagree with its rows")
    return dict(marker), [dict(row) for row in rows]


# ---------------------------------------------------------------------- writer


def build_batch(
    sources: Sequence[Any],
    *,
    producer: str,
    run_id: str,
    hermit_sha: str,
    emitted_at: str,
    source_tree_dirty: bool,
    host: str,
    team: str = TEAM,
) -> list[dict]:
    """Wrap one run's sources: the run marker, then one row per source.

    A source whose own run_id or hermit_sha (from its node's status) differs
    from the run's is refused, as is anything carrying an owner path: the
    ledger is public.
    """
    _choice(producer, PRODUCERS, "--producer")
    if not isinstance(run_id, str) or not run_id.strip() or "\0" in run_id:
        raise ParityLedgerError(f"--run-id must be non-empty, got {run_id!r}")
    if not isinstance(hermit_sha, str) or _SHA1_RE.fullmatch(hermit_sha) is None:
        raise ParityLedgerError(f"--tree must be 40 lowercase hex digits, got {hermit_sha!r}")
    if not isinstance(emitted_at, str) or _EMITTED_AT_RE.fullmatch(emitted_at) is None:
        raise ParityLedgerError(f"--emitted-at must be YYYY-MM-DDTHH:MM:SSZ, got {emitted_at!r}")
    _segment(team, "team")
    _segment(host, "host")
    _boolean(source_tree_dirty, "source_tree_dirty")
    rows: list[dict] = []
    for index, source in enumerate(sources, 1):
        try:
            value = validate_source(_public_first_difference(source))
        except ParityLedgerError as error:
            raise ParityLedgerError(f"input row {index}: {error}") from error
        at = f"input row {index}: parity ledger source {value['cell']}"
        if value["run_id"] is not None and value["run_id"] != run_id:
            raise ParityLedgerError(
                f"{at}: its node's run_id {value['run_id']!r} is not --run-id {run_id!r}"
            )
        if value["hermit_sha"] is not None and value["hermit_sha"] != hermit_sha:
            raise ParityLedgerError(
                f"{at}: its node's hermit_sha {value['hermit_sha']!r} is not --tree {hermit_sha!r}"
            )
        if contains_owner_path(value):
            raise ParityLedgerError(
                f"{at}: contains a workspace-local owner path; the parity ledger is public"
            )
        check_producer_class(at, producer, value["unavailable_class"])
        origin = value["source"]
        row = {
            "schema": SCHEMA,
            "event_type": ROW_EVENT_TYPE,
            "event_id": event_id(producer, run_id, value["cell"], origin["lane"], origin["node"]),
            "team": team,
            "host": host,
            "emitted_at": emitted_at,
            "producer": producer,
            "run_id": run_id,
            "hermit_sha": hermit_sha,
            "source_tree_dirty": source_tree_dirty,
            "cell": value["cell"],
            "test_id": value["test_id"],
            "backend": value["backend"],
            "verdict": value["verdict"],
            "unavailable_class": value["unavailable_class"],
            "operand": value["operand"],
            "reason": value["reason"],
            "source": {field: origin[field] for field in ORIGIN_FIELDS},
            "record": _canonical_record(value["record"]),
        }
        rows.append(canonical_event(row))
    marker = {
        "schema": SCHEMA,
        "event_type": RUN_EVENT_TYPE,
        "event_id": run_event_id(producer, run_id),
        "team": team,
        "host": host,
        "emitted_at": emitted_at,
        "producer": producer,
        "run_id": run_id,
        "hermit_sha": hermit_sha,
        "source_tree_dirty": source_tree_dirty,
        "records": len(rows),
        "verdicts": _verdict_counts(rows),
        "nodes": _run_nodes(rows),
    }
    events = [marker, *rows]
    validate_batch(events, "parity batch being written")
    return events


def batch_name(marker: Mapping[str, Any]) -> str:
    """`<team>__<host>__<YYYY-MM>__<run event id>.jsonl`: one file per run."""
    return (
        f"{marker['team']}__{marker['host']}__{str(marker['emitted_at'])[:7]}__"
        f"{marker['event_id']}.jsonl"
    )


def shard_path(root: str, team: str, host: str, month: str, part: int = 0) -> str:
    """`<root>/<team>/<host>/<YYYY-MM>.jsonl`, a month's first file, or for
    `part` 1 and later `<YYYY-MM>.part-<NNNN>.jsonl`, the parts it continues in
    once a file would pass the publisher's ceiling ([`plan_publication`]), the
    naming the series and validation ledgers use (ledger/README.md). Part
    numbers have exactly four digits, so a month's files in path order hold its
    events in publication order."""
    if root not in (ROW_ROOT, RUN_ROOT, QUARANTINE_RETRACTION_ROOT):
        raise ParityLedgerError(f"unknown parity tree {root!r}")
    if re.fullmatch(r"\d{4}-\d{2}", month or "") is None:
        raise ParityLedgerError(f"month must be YYYY-MM, got {month!r}")
    _segment(team, "team")
    _segment(host, "host")
    if not 0 <= part <= 9999:
        raise ParityLedgerError(f"part must be 0-9999, got {part!r}")
    if part == 0:
        return f"{root}/{team}/{host}/{month}.jsonl"
    return f"{root}/{team}/{host}/{month}.part-{part:04d}.jsonl"


def shard_part(relative: str) -> tuple[str, int] | None:
    """The month's first file and the part number of `relative`, a file of one
    of the trees this store publishes (`parity/`, `parity-runs/` or
    `quarantine/retractions/`), or None for any other path."""
    parts = Path(relative).parts
    for root in (ROW_ROOT, RUN_ROOT, QUARANTINE_RETRACTION_ROOT):
        prefix = Path(root).parts
        if len(parts) == len(prefix) + 3 and parts[:len(prefix)] == prefix:
            named = re.fullmatch(r"(\d{4}-\d{2})(?:\.part-(\d{4}))?\.jsonl", parts[-1])
            if named is None:
                return None
            part = int(named.group(2) or 0)
            if named.group(2) is not None and part == 0:
                return None
            try:
                if shard_path(root, parts[-3], parts[-2], named.group(1), part) != relative:
                    return None
                return shard_path(root, parts[-3], parts[-2], named.group(1)), part
            except ParityLedgerError:
                return None
    return None


def is_shard_path(relative: str) -> bool:
    """A file of one of the trees this store publishes, a month's first file
    or one of its parts ([`shard_path`])."""
    return shard_part(relative) is not None


def event_shard(event: Mapping[str, Any]) -> str:
    """The first file of the month `event` belongs to. The event is in that
    file or in one of its parts ([`shard_path`])."""
    root = (RUN_ROOT if event.get("event_type") in RUN_LEVEL_EVENT_TYPES
            else QUARANTINE_RETRACTION_ROOT
            if event.get("event_type") == QUARANTINE_RETRACTION_EVENT_TYPE else ROW_ROOT)
    return shard_path(root, event["team"], event["host"], str(event["emitted_at"])[:7])


def encode_batch(events: Sequence[Mapping[str, Any]]) -> str:
    return "".join(canonical_line(event) + "\n" for event in events)


def parse_lines(text: str, label: str) -> list[dict]:
    """Events of one JSONL text, each line required in its canonical encoding."""
    if text and not text.endswith("\n"):
        raise ParityLedgerError(f"{label}: final line has no newline terminator")
    events = []
    for number, line in enumerate(text.split("\n")[:-1] if text else [], 1):
        if not line.strip():
            raise ParityLedgerError(f"{label}:{number}: blank line")
        value = loads(line, f"{label}:{number}")
        if not isinstance(value, dict):
            raise ParityLedgerError(f"{label}:{number}: not a JSON object")
        if canonical_line(value) != line:
            raise ParityLedgerError(f"{label}:{number}: not the canonical compact encoding")
        events.append(value)
    return events


def read_batch(path: Path) -> tuple[dict, list[dict]]:
    """One spooled batch file, fully validated."""
    try:
        text = Path(path).read_bytes().decode("utf-8")
    except (OSError, UnicodeDecodeError) as error:
        raise ParityLedgerError(f"{path.name}: cannot read: {error}") from error
    events = parse_lines(text, path.name)
    marker, rows = validate_batch(events, path.name)
    if batch_name(marker) != path.name:
        raise ParityLedgerError(f"{path.name}: its run marker names the batch {batch_name(marker)}")
    return marker, rows


def batch_paths(spool: Path) -> list[Path]:
    return sorted(Path(spool).glob("*.jsonl"))


def without_emitted_at(event: Mapping[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in event.items() if key != "emitted_at"}


def same_event(left: Mapping[str, Any], right: Mapping[str, Any]) -> bool:
    """Equal bytes, ignoring only when the event was written."""
    return canonical_line(without_emitted_at(left)) == canonical_line(without_emitted_at(right))


def _retention_module():
    directory = Path(__file__).resolve().parents[1] / "ledger"
    ledger = str(directory)
    if ledger not in sys.path:
        sys.path.insert(0, ledger)
    import ledger_retention
    if Path(str(getattr(ledger_retention, "__file__", ""))).resolve() != directory / "ledger_retention.py":
        raise ParityLedgerError("archive reader was imported from a different tool tree")
    return ledger_retention


def _file_archive_module():
    directory = Path(__file__).resolve().parents[1] / "ledger"
    if str(directory) not in sys.path:
        sys.path.insert(0, str(directory))
    import file_archive
    if Path(str(getattr(file_archive, "__file__", ""))).resolve() != directory / "file_archive.py":
        raise ParityLedgerError("file archive reader was imported from a different tool tree")
    return file_archive


def _already_archived(events: Sequence[Mapping[str, Any]], published: Mapping[str, dict],
                      archived: Mapping[str, dict] | None = None) -> bool:
    """Recognize an exact whole-run replay without inventing archived event bodies."""
    if isinstance(published, _PublishedEvents):
        published.verify()
    archived = getattr(published, "archived_events", {}) if archived is None else archived
    if not any(event["event_id"] in archived for event in events):
        return False
    retention = _retention_module()
    for event in events:
        descriptor = archived.get(event["event_id"])
        if descriptor is None:
            raise ParityLedgerError("only part of the run is archived; a run is published whole or not at all")
        if (descriptor["event_type"] != event["event_type"]
                or descriptor["event_hash"] != retention.event_hash(event)):
            raise ParityLedgerError(f"{_describe(event)} is already archived with a different body")
        live = published.get(event["event_id"])
        if live is not None and not same_event(live, event):
            raise ParityLedgerError(f"{_describe(event)} is already published with a different body")
    return True


def _first_difference(existing: Mapping[str, dict], fresh: Sequence[dict]) -> str:
    ids = {event["event_id"] for event in fresh}
    for event in fresh:
        prior = existing.get(event["event_id"])
        if prior is None:
            return f"{_describe(event)} is new, but part of the run is already recorded"
        if not same_event(prior, event):
            return f"{_describe(event)} differs from the recorded copy"
    for event_id_value, prior in existing.items():
        if event_id_value not in ids:
            return f"{_describe(prior)} is recorded but not in this batch"
    return "no difference"


def _describe(event: Mapping[str, Any]) -> str:
    if event.get("event_type") in RETRACTION_EVENT_TYPES:
        return f"retraction {event.get('event_id')} of {len(event.get('retracts') or [])} event(s)"
    if event.get("event_type") == REFUSAL_EVENT_TYPE:
        return f"refusal of {event.get('producer')} run {event.get('run_id')}"
    if event.get("event_type") == RUN_EVENT_TYPE:
        return (f"run marker of {event.get('producer')} run {event.get('run_id')} "
                f"({event.get('records')} record(s))")
    source = event.get("source")
    if not isinstance(source, Mapping):
        source = {}
    return f"row {event.get('cell')} from node {source.get('lane')}/{source.get('node')}"


def recorded_run_events(
    spool: Path, run_id_value: str, *, published: Mapping[str, dict] | None = None,
) -> tuple[dict[str, dict], list[str]]:
    """Every already-recorded event of one run: pending, retained, published.

    A run's events all live in the one batch named by its run event id, so
    only that file is read from the spool and its `published/` directory.
    """
    found: dict[str, dict] = {}
    where: list[str] = []
    for directory in (Path(spool), Path(spool) / "published"):
        for path in sorted(directory.glob(f"*__{run_id_value}.jsonl")):
            marker, rows = read_batch(path)
            for event in (marker, *rows):
                found.setdefault(event["event_id"], event)
            where.append(str(path.relative_to(Path(spool))))
    return found, where


def append_parity(
    spool: Path,
    sources: Sequence[Any],
    *,
    producer: str,
    run_id: str,
    hermit_sha: str,
    emitted_at: str,
    source_tree_dirty: bool,
    host: str,
    published: Mapping[str, dict] | None = None,
    published_check: str | None = None,
) -> dict[str, Any]:
    """Validate one run's sources and spool its batch; caller holds the lock.

    Replaying a run whose rows are already recorded (pending, retained, or in
    `published`) with only a different `emitted_at` writes nothing. Any other
    difference is refused: the ledger is append-only and a changed record is
    never replaced.
    """
    events = build_batch(
        sources, producer=producer, run_id=run_id, hermit_sha=hermit_sha,
        emitted_at=emitted_at, source_tree_dirty=source_tree_dirty, host=host,
    )
    marker = events[0]
    existing, where = recorded_run_events(spool, marker["event_id"])
    if published:
        for event in events:
            prior = published.get(event["event_id"])
            if prior is not None:
                existing.setdefault(event["event_id"], prior)
        if any(event["event_id"] in published for event in events):
            where.append("the published ledger")
    result = {
        "schema": "parity-append/v1",
        "producer": producer,
        "run_id": run_id,
        "hermit_sha": hermit_sha,
        "records": marker["records"],
        "verdicts": dict(marker["verdicts"]),
        "batch": None,
        "already_recorded": False,
        "published_check": published_check,
    }
    if published is not None and _already_archived(events, published):
        fresh_by_id = {event["event_id"]: event for event in events}
        if (not set(existing) <= fresh_by_id.keys()
                or any(not same_event(prior, fresh_by_id[eid]) for eid, prior in existing.items())):
            raise ParityLedgerError(
                f"{producer} run {run_id} has a different locally recorded body: "
                f"{_first_difference(existing, events)}"
            )
        result["already_recorded"] = True
        result["recorded_in"] = [*where, "the published archive manifest"]
        return result
    if existing:
        fresh_ids = {event["event_id"] for event in events}
        if set(existing) == fresh_ids and all(
            same_event(existing[event["event_id"]], event) for event in events
        ):
            result["already_recorded"] = True
            result["recorded_in"] = where
            return result
        raise ParityLedgerError(
            f"{producer} run {run_id} is already recorded in {', '.join(where)} with different "
            f"content: {_first_difference(existing, events)}; the parity ledger is append-only, "
            "so a changed record is refused rather than replaced"
        )
    result["batch"] = _write_batch(Path(spool), events)
    return result


def _write_batch(spool: Path, events: Sequence[Mapping[str, Any]]) -> str:
    """Write one batch atomically under its own name and read it back."""
    spool.mkdir(parents=True, exist_ok=True)
    name = batch_name(events[0])
    target = spool / name
    temporary = spool / f".{name}.{os.getpid()}.tmp"
    data = encode_batch(events).encode("utf-8")
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC, 0o644)
    try:
        view = memoryview(data)
        while view:
            written = os.write(descriptor, view)
            view = view[written:]
        os.fsync(descriptor)
    except BaseException:
        os.close(descriptor)
        temporary.unlink(missing_ok=True)
        raise
    os.close(descriptor)
    # Publishers glob only `*.jsonl`, so the batch appears whole or not at all.
    os.replace(temporary, target)
    read_batch(target)
    return name


def spool_retraction(spool: Path, event: Mapping[str, Any]) -> str:
    """Spool one `parity.retraction` as a batch of its own; caller holds the lock.

    The publisher appends it to `parity/<team>/<host>/<YYYY-MM>.jsonl` only
    when every event it names is already published (`plan_publication`).
    """
    validate_retraction(event)
    # The identity leaves out emitted_at, so the same withdrawal spooled again
    # in the same month names the same batch: keep the first copy rather than
    # replace it, and refuse a different one under that name.
    existing = Path(spool) / batch_name(event)
    if existing.exists():
        prior, _rows = read_batch(existing)
        if not same_event(prior, event):
            raise ParityLedgerError(f"{existing.name} is already spooled with different content")
        return existing.name
    return _write_batch(Path(spool), [dict(event)])


def build_refusal(
    *,
    producer: str,
    run_id: str,
    hermit_sha: str,
    emitted_at: str,
    source_tree_dirty: bool | None,
    host: str,
    input_bytes: bytes,
    input_lines: int | None,
    error: Any,
    team: str = TEAM,
) -> dict:
    """The `parity.refusal` event for one refused `append-parity` call."""
    message = public_error(error)
    input_sha256 = hashlib.sha256(input_bytes).hexdigest()
    event = canonical_event({
        "schema": SCHEMA,
        "event_type": REFUSAL_EVENT_TYPE,
        "event_id": refusal_event_id(producer, run_id, input_sha256, message),
        "team": team,
        "host": host,
        "emitted_at": emitted_at,
        "producer": producer,
        "run_id": run_id,
        "hermit_sha": hermit_sha,
        "source_tree_dirty": source_tree_dirty,
        "input_sha256": input_sha256,
        "input_lines": input_lines,
        "error": message,
    })
    validate_refusal(event)
    return event


def append_refusal(spool: Path, event: Mapping[str, Any]) -> dict[str, Any]:
    """Spool one refusal as a batch of its own; caller holds the lock.

    The same input refused for the same reason is one event, so a replay that
    finds its refusal already spooled or retained writes nothing.
    """
    validate_refusal(event)
    for directory in (Path(spool), Path(spool) / "published"):
        for path in sorted(directory.glob(f"*__{event['event_id']}.jsonl")):
            prior, _ = read_batch(path)
            if same_event(prior, event):
                return {"batch": None, "recorded_in": str(path.relative_to(Path(spool)))}
    return {"batch": _write_batch(Path(spool), [event]), "recorded_in": None}


def run_refusals(
    spool: Path, producer: str, run_id_value: str, *,
    published: Mapping[str, Mapping[str, Any]] | None = None,
) -> list[dict]:
    """Every recorded refusal of one run, oldest first.

    Read from the spool, its `published/` retention and, when given, the
    published events. A refusal's batch is named by its own event id, which
    depends on the refused input, so every spooled batch is read; one that
    cannot be read is skipped here (the publisher names it as refused).
    """
    found: dict[str, dict] = {}
    for directory in (Path(spool), Path(spool) / "published"):
        for path in batch_paths(directory):
            try:
                event, rows = read_batch(path)
            except ParityLedgerError:
                continue
            if event.get("event_type") == REFUSAL_EVENT_TYPE and not rows:
                found.setdefault(event["event_id"], event)
    for event in (published or {}).values():
        if event.get("event_type") == REFUSAL_EVENT_TYPE:
            try:
                validate_refusal(event)
            except ParityLedgerError:
                continue
            found.setdefault(event["event_id"], dict(event))
    return sorted(
        (event for event in found.values()
         if event["producer"] == producer and event["run_id"] == run_id_value),
        key=lambda event: (event["emitted_at"], event["event_id"]),
    )


# -------------------------------------------------------------------- publish


@dataclass(frozen=True)
class PublishedParity:
    """Every parity event at one ledger commit, and the shard texts read.

    ``events`` indexes the published events by id, for the duplicate and
    conflict checks a new batch needs. Published events are indexed, not
    revalidated: the ledger is append-only, so a row that a later, stricter
    validator would refuse can never be repaired, and it must not stop every
    later publication and reap. What the index itself cannot use (a line that
    is not a JSON object with an event id, an event in the wrong shard, an id
    published twice with different bodies, a path outside the shard layout)
    is listed in ``invalid`` with its location and reported, never raised.
    Every event being appended is validated in full by `read_batch`.
    """

    commit: str
    events: Mapping[str, dict]
    texts: dict[str, str]
    invalid: tuple[dict[str, Any], ...] = ()
    archived_events: Mapping[str, dict] = field(default_factory=dict)
    archived_runs: Mapping[tuple[str, str, str], dict] = field(default_factory=dict)
    retired_files: Mapping[str, dict] = field(default_factory=dict)
    files: Any = field(default=None, repr=False, compare=False)

    def verify(self) -> None:
        if self.files is not None:
            self.files.verify(self.files.listing((QUARANTINE_RETRACTION_ROOT,)))

    def close(self) -> None:
        if self.files is not None:
            self.files.close()


class _PublishedEvents(Mapping):
    """The published events by id, each parsed from its shard text when it is read.

    Holding every event as a parsed object cost about 8 bytes of memory per
    byte of parity text: about 13 GB in each publication on 2026-10-11, for
    1.68 GB of text growing about 0.6 GB a day
    (https://github.com/rrnewton/dev-hermit/issues/554). The texts are held
    anyway, so the index keeps only where each event's line is. A lookup parses
    that line with `loads`, giving an object equal to the one the full index
    held.
    """

    __slots__ = ("_texts", "_spans", "archived_events", "_files")

    def __init__(self, texts: Mapping[str, str], spans: dict[str, tuple[str, int, int]],
                 archived_events: Mapping[str, dict] | None = None, files: Any = None) -> None:
        self._texts = texts
        self._spans = spans
        self.archived_events = archived_events or {}
        self._files = files

    def verify(self) -> None:
        if self._files is not None:
            self._files.verify(self._files.listing((QUARANTINE_RETRACTION_ROOT,)))

    def __getitem__(self, event_id: str) -> dict:
        relative, start, end = self._spans[event_id]
        if self._files is not None and relative.startswith(QUARANTINE_RETRACTION_ROOT + "/"):
            self._files.verify((relative,))
        return loads(self._texts[relative][start:end], relative)

    def __contains__(self, event_id: object) -> bool:
        return event_id in self._spans

    def __iter__(self):
        return iter(self._spans)

    def __len__(self) -> int:
        return len(self._spans)


def _git(repo: Path, *args: str) -> subprocess.CompletedProcess:
    env = {name: value for name, value in os.environ.items() if not name.startswith("GIT_")}
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    return subprocess.run(
        ["git", "--no-replace-objects", "-C", str(repo), *args],
        capture_output=True, text=True, timeout=120, check=False, env=env,
    )


def shard_events(text: str, label: str) -> list[dict]:
    """Events of one shard text: markers, refusals or rows, each validated."""
    events = parse_lines(text, label)
    for index, event in enumerate(events, 1):
        try:
            validate_event(event)
        except ParityLedgerError as error:
            raise ParityLedgerError(f"{label}:{index}: {error}") from error
    return events


def _index_lines(text: str, relative: str, invalid: list[dict[str, Any]]) -> Iterator[tuple[int, dict, int, int]]:
    """The indexable events of one published shard, with where each line is in TEXT.

    Yields ``(line number, event, start, end)``, one line at a time; anything
    else goes to ``invalid``. A final line without a newline is reported before
    any event, as when the whole text was split first.
    """
    count = text.count("\n")
    if text and not text.endswith("\n"):
        invalid.append({"path": relative, "line": count + 1,
                        "error": "final line has no newline terminator"})
    start = 0
    number = 0
    while start < len(text):
        end = text.find("\n", start)
        if end < 0:
            end = len(text)
        number += 1
        line = text[start:end]
        try:
            value = loads(line, f"{relative}:{number}")
        except ParityLedgerError as error:
            invalid.append({"path": relative, "line": number, "error": str(error)})
        else:
            if not isinstance(value, dict) or not isinstance(value.get("event_id"), str) or not value["event_id"]:
                invalid.append({"path": relative, "line": number,
                                "error": "not a JSON object with an event_id"})
            else:
                yield number, value, start, end
        start = end + 1


def published_parity_at_repo(repo: Path, ref: str) -> PublishedParity:
    """Index `parity/` and `parity-runs/` at one ledger ref.

    Raises only when the ref or a shard cannot be read at all; see
    `PublishedParity` for what is indexed and what is reported.
    """
    return _published_parity(repo, ref, None)


def published_parity_shards(repo: Path, ref: str, shards: Iterable[str]) -> PublishedParity:
    """`published_parity_at_repo` limited to SHARDS: only those that exist at REF are read.

    An event whose id also appears in a shard not read is indexed from the
    shards read alone, so a caller that compares two refs this way sees such a
    copy as new, never as already published.
    """
    return _published_parity(repo, ref, frozenset(shards))


class _ShardTexts(Mapping):
    """Every shard path at one commit; a shard's text is read from Git when first asked for."""

    __slots__ = ("_repo", "_commit", "_paths", "_read", "_files")

    def __init__(self, repo: Path, commit: str, paths: Sequence[str], read: dict[str, str],
                 files: Any = None) -> None:
        self._repo = Path(repo)
        self._commit = commit
        self._paths = frozenset(paths)
        self._read = dict(read)
        self._files = files

    def __getitem__(self, relative: str) -> str:
        if relative not in self._paths:
            raise KeyError(relative)
        if self._files is not None and relative.startswith(QUARANTINE_RETRACTION_ROOT + "/"):
            self._files.verify((relative,))
            if relative not in self._read:
                data = self._files.read_file(relative)
                if data is None:
                    raise ParityLedgerError(f"original retraction file is missing: {relative}")
                self._read[relative] = data.decode("utf-8")
        if relative not in self._read:
            shown = _git(self._repo, "show", f"{self._commit}:{relative}")
            if shown.returncode != 0:
                raise ParityLedgerError(
                    f"cannot read {self._commit}:{relative}: {shown.stderr.strip() or 'no output'}"
                )
            self._read[relative] = shown.stdout
        return self._read[relative]

    def __contains__(self, relative: object) -> bool:
        return relative in self._paths

    def __iter__(self):
        return iter(sorted(self._paths))

    def __len__(self) -> int:
        return len(self._paths)


def _highest_part(family: str, paths: Iterable[str]) -> str:
    """The highest published part of FAMILY among PATHS, or FAMILY itself."""
    parts = [(shard_part(relative) or (None, 0))[1] for relative in paths
             if (shard_part(relative) or (None, 0))[0] == family]
    if not parts or max(parts) == 0:
        return family
    root, team, host, name = family.rsplit("/", 3)
    return shard_path(root, team, host, name.removesuffix(".jsonl"), max(parts))


def published_parity_for_batches(repo: Path, ref: str, batches: Sequence[Path], *,
                                  files: Any = None) -> PublishedParity:
    """The published parity that publishing or reaping BATCHES consults, read in bounded memory.

    `published_parity_at_repo` reads every shard. The parity ledger was 1.70 GB
    of text on 2026-10-11 and grows about 0.6 GB a day, so that read grew with
    it (https://github.com/rrnewton/dev-hermit/issues/554). This reads only:

    - every run-level shard (`parity-runs/`: run markers and refusals) and
      every quarantine-retraction shard, a few MB in all;
    - the highest part of each row family a batch appends to, which is the
      text a plan extends and the size its ceiling counts;
    - for a batch whose run marker is already published, the row shards of
      the commit that published that marker.

    A run is published whole, in one commit (`plan_publication`), so a
    published marker means its rows are published, in shards that commit
    changed. An unpublished marker means none of its rows are. For every batch
    whose events a publication or a reap looks up, that answers exactly what
    the whole index does. A retraction names any published event, so a batch
    of retractions makes this read the whole ledger, as before. `texts` names
    every shard at REF and reads one only when asked; `invalid` covers the
    shards read.
    """
    pending: list[list[dict]] = []
    for path in batches:
        try:
            marker, rows = read_batch(path)
        except ParityLedgerError:
            continue  # the plan and the reap refuse it by name; nothing to look up
        pending.append([marker, *rows])
    return published_parity_for_runs(repo, ref, pending, files=files)


def published_parity_for_runs(repo: Path, ref: str, runs: Sequence[Sequence[Mapping[str, Any]]],
                              *, files: Any = None,
                              ) -> PublishedParity:
    """`published_parity_for_batches` for batches already read: each run's marker, then its rows.

    `series.py append-parity` looks up the run it is about to spool this way.
    On 2026-10-14 the whole index (`published_parity_at_repo`, 1,558,814
    events) took 68.7 s and 3.7 GB on the verdict path of every Hermit
    validation; this read answered the same for that run's 2,921 events in
    3.6 s.
    """
    resolved = _git(Path(repo), "rev-parse", "--verify", f"{ref}^{{commit}}")
    commit = resolved.stdout.strip()
    if resolved.returncode != 0 or _SHA1_RE.fullmatch(commit) is None:
        raise ParityLedgerError(
            f"cannot resolve published parity revision {ref}: {resolved.stderr.strip() or 'no output'}"
        )
    listed = _git(Path(repo), "ls-tree", "-r", "--name-only", commit, "--", ROW_ROOT, RUN_ROOT,
                  QUARANTINE_RETRACTION_ROOT)
    if listed.returncode != 0:
        raise ParityLedgerError(
            f"cannot list parity shards at {commit}: {listed.stderr.strip() or 'no output'}"
        )
    owned_files = files is None
    try:
        try:
            files = files if files is not None else _file_archive_module().FileArchiveSnapshot(Path(repo), commit)
            if files.commit != commit:
                raise ValueError("file archive snapshot names another commit")
            paths = sorted(set(listed.stdout.splitlines())
                           | set(files.listing((QUARANTINE_RETRACTION_ROOT,))))
        except (ValueError, OSError, subprocess.SubprocessError) as error:
            raise ParityLedgerError(f"cannot read file archives at {commit}: {error}") from error
        shards = [relative for relative in paths if is_shard_path(relative)]
        run_level = [relative for relative in shards if not relative.startswith(ROW_ROOT + "/")]
        pending = [list(events) for events in runs if events]
        if any(events[0].get("event_type") in RETRACTION_EVENT_TYPES for events in pending):
            return _published_parity(repo, commit, None, files=files)
        markers = _published_parity(repo, commit, frozenset(run_level), files=files)
        wanted = set(run_level)
        for events in pending:
            for event in events:
                try:
                    wanted.add(_highest_part(event_shard(event), shards))
                except (KeyError, TypeError, ParityLedgerError):
                    continue  # read_batch accepted it; event_shard cannot fail on it
            located = markers.events._spans.get(events[0]["event_id"])
            if located is not None and events[0]["event_type"] == RUN_EVENT_TYPE:
                relative, start, _end = located
                line = markers.texts[relative].count("\n", 0, start) + 1
                blamed = _git(Path(repo), "blame", "--porcelain", "-L", f"{line},{line}", commit, "--",
                              relative)
                introduced = blamed.stdout.split(" ", 1)[0] if blamed.returncode == 0 else ""
                if _SHA1_RE.fullmatch(introduced) is None:
                    raise ParityLedgerError(
                        f"cannot find the commit that published {_describe(events[0])}: "
                        f"{blamed.stderr.strip() or 'no output'}"
                    )
                touched = _git(Path(repo), "diff-tree", "--no-commit-id", "--name-only", "-r", "--root",
                               introduced, "--", ROW_ROOT)
                if touched.returncode != 0:
                    raise ParityLedgerError(
                        f"cannot list the shards {introduced} changed: {touched.stderr.strip() or 'no output'}"
                    )
                wanted.update(relative for relative in touched.stdout.splitlines() if relative in shards)
        read = _published_parity(repo, commit, frozenset(wanted), files=files)
        return PublishedParity(commit=commit, events=read.events,
                               texts=_ShardTexts(repo, commit, shards, read.texts, files=read.files),
                               invalid=read.invalid, archived_events=read.archived_events,
                               archived_runs=read.archived_runs, retired_files=read.retired_files,
                               files=read.files)
    except BaseException:
        if owned_files and files is not None:
            files.close()
        raise


def _published_parity(repo: Path, ref: str, only: frozenset[str] | None, *,
                      files: Any = None) -> PublishedParity:
    resolved = _git(Path(repo), "rev-parse", "--verify", f"{ref}^{{commit}}")
    commit = resolved.stdout.strip()
    if resolved.returncode != 0 or _SHA1_RE.fullmatch(commit) is None:
        raise ParityLedgerError(
            f"cannot resolve published parity revision {ref}: {resolved.stderr.strip() or 'no output'}"
        )
    listed = _git(Path(repo), "ls-tree", "-r", "--name-only", commit, "--", ROW_ROOT, RUN_ROOT,
                  QUARANTINE_RETRACTION_ROOT)
    if listed.returncode != 0:
        raise ParityLedgerError(
            f"cannot list parity shards at {commit}: {listed.stderr.strip() or 'no output'}"
        )
    owned_files = files is None
    try:
        try:
            files = files if files is not None else _file_archive_module().FileArchiveSnapshot(Path(repo), commit)
            if files.commit != commit:
                raise ValueError("file archive snapshot names another commit")
            quarantine_paths = set(files.listing((QUARANTINE_RETRACTION_ROOT,)))
        except (ValueError, OSError, subprocess.SubprocessError) as error:
            raise ParityLedgerError(f"cannot read file archives at {commit}: {error}") from error
        spans: dict[str, tuple[str, int, int]] = {}
        texts: dict[str, str] = {}
        invalid: list[dict[str, Any]] = []
        for relative in sorted(set(listed.stdout.splitlines()) | quarantine_paths):
            if only is not None and relative not in only:
                continue
            if not is_shard_path(relative):
                invalid.append({"path": relative, "line": None, "error": "not a parity shard path; not read"})
                continue
            if relative in quarantine_paths:
                try:
                    data = files.read_file(relative)
                    if data is None:
                        raise ValueError("original retraction file is missing")
                    text = data.decode("utf-8")
                except (ValueError, OSError) as error:
                    raise ParityLedgerError(f"cannot read {commit}:{relative}: {error}") from error
            else:
                shown = _git(Path(repo), "show", f"{commit}:{relative}")
                if shown.returncode != 0:
                    raise ParityLedgerError(
                        f"cannot read {commit}:{relative}: {shown.stderr.strip() or 'no output'}"
                    )
                text = shown.stdout
            texts[relative] = text
            for number, event, start, end in _index_lines(text, relative, invalid):
                try:
                    placed = event_shard(event)
                except (KeyError, TypeError, ParityLedgerError) as error:
                    placed = f"no shard ({type(error).__name__}: {error})"
                if placed != shard_part(relative)[0]:
                    invalid.append({"path": relative, "line": number,
                                    "error": f"{_describe(event)} belongs in {placed}"})
                first = spans.get(event["event_id"])
                if first is not None:
                    prior = loads(texts[first[0]][first[1]:first[2]], first[0])
                    if canonical_line(prior) != canonical_line(event):
                        invalid.append({"path": relative, "line": number,
                                        "error": f"event_id {event['event_id']} is published twice with "
                                                 "different bodies; the first copy stands"})
                    continue
                spans[event["event_id"]] = (relative, start, end)
        try:
            files.verify(quarantine_paths)
            archives = files.archive_index
        except (ValueError, OSError, subprocess.SubprocessError) as error:
            raise ParityLedgerError(f"cannot read archive manifest at {commit}: {error}") from error
        return PublishedParity(
            commit=commit, events=_PublishedEvents(texts, spans, archives.archived_events, files), texts=texts,
            invalid=tuple(invalid), archived_events=archives.archived_events,
            archived_runs=archives.archived_runs, retired_files=archives.retired_files, files=files,
        )
    except BaseException:
        if owned_files and files is not None:
            files.close()
        raise


@dataclass
class PublicationPlan:
    """What one publication attempt appends, and what it cannot."""

    texts: dict[str, str]
    appended: int
    fresh_batches: list[Path]
    already_published: list[Path]
    refused: list[dict[str, str]]
    #: The published text of every shard in ``texts``, which each merged text
    #: must extend line for line.
    base_texts: dict[str, str]


def plan_publication(
    batches: Sequence[Path], published: PublishedParity, *, ceiling: int | None = None,
    quarantine_ids: AbstractSet[str] | None = None,
) -> PublicationPlan:
    """Merge every pending batch that can be published onto `published`.

    A batch is refused (and left spooled) when it is unreadable, when one of
    its events is already published with a different body, when only part of
    it is published, or when it collides with an earlier batch. A batch whose
    every event is already published, differing at most in `emitted_at`, is
    already published: the first published copy stands.

    With a `ceiling`, no file grows past that many bytes. The publisher passes
    its bound on one published file: these files ride in the series commit,
    and GitHub refuses a whole push carrying a file over 104,857,600 bytes. A
    month's events are appended to its highest part ([`shard_path`]), and the
    first event that would take that part past the ceiling starts the next
    part, as the series and validation ledgers do. Manifest-retired paths stay
    reserved, including a month whose entire published tree was retired. A
    late batch starts beyond its last retired part. Only an event that alone
    is over the ceiling is refused. A batch is still published whole or not
    at all; its events may span two parts.
    """
    published.verify()
    texts: dict[str, str] = {}
    sizes: dict[str, int] = {}
    latest: dict[str, tuple[str, int]] = {}
    for relative in (*published.texts, *published.retired_files):
        parsed = shard_part(relative)
        if parsed is not None:
            family, part = parsed
            if family not in latest or part > latest[family][1]:
                latest[family] = relative, part

    def last_file(family: str) -> str:
        """Append to the newest live part or allocate after any retired part."""
        relative, part = latest.get(family, (family, 0))
        if relative not in published.retired_files:
            return relative
        root, team, host, name = family.rsplit("/", 3)
        return shard_path(root, team, host, name.removesuffix(".jsonl"), part + 1)

    def size_of(relative: str) -> int:
        if relative not in sizes:
            sizes[relative] = len(texts.get(relative, published.texts.get(relative, "")).encode())
        return sizes[relative]

    fresh: list[Path] = []
    already: list[Path] = []
    refused: list[dict[str, str]] = []
    claimed: dict[str, dict] = {}
    appended = 0
    for path in batches:
        try:
            marker, rows = read_batch(path)
            events = [marker, *rows]
            if _already_archived(events, published.events, published.archived_events):
                already.append(path)
                continue
            prior = [published.events.get(event["event_id"]) for event in events]
            if all(item is not None for item in prior):
                changed = [event for event, item in zip(events, prior) if not same_event(item, event)]
                if changed:
                    raise ParityLedgerError(
                        f"{_describe(changed[0])} is already published with a different body"
                    )
                already.append(path)
                continue
            if any(item is not None for item in prior):
                count = sum(item is not None for item in prior)
                raise ParityLedgerError(
                    f"{count} of its {len(events)} event(s) are already published; a run is "
                    "published whole or not at all"
                )
            for event in events:
                if event["event_id"] in claimed:
                    raise ParityLedgerError(
                        f"{_describe(event)} is also in another pending batch"
                    )
                target = last_file(event_shard(event))
                base = published.texts.get(target, "")
                if base and not base.endswith("\n"):
                    raise ParityLedgerError(
                        f"published shard {target} has no final newline, so "
                        "nothing can be appended to it"
                    )
            if marker["event_type"] == RETRACTION_EVENT_TYPE:
                # A retraction withdraws only what is already published, and
                # never another retraction: withdrawing a withdrawal would make
                # what a reader drops depend on the order it reads them in.
                for item in marker["retracts"]:
                    target = published.events.get(item)
                    if target is None:
                        target = published.archived_events.get(item)
                        if target is None:
                            raise ParityLedgerError(
                                f"{_describe(marker)} names {item}, which is not a published parity event"
                            )
                    if target.get("event_type") in RETRACTION_EVENT_TYPES:
                        raise ParityLedgerError(
                            f"{_describe(marker)} names {item}, which is itself a retraction"
                        )
            if marker["event_type"] == QUARANTINE_RETRACTION_EVENT_TYPE:
                # A quarantine retraction withdraws only published refusals;
                # its ids cannot name a retraction (their form differs).
                if quarantine_ids is None:
                    raise ParityLedgerError(
                        f"{_describe(marker)} withdraws series quarantine records, but the "
                        "published quarantine records were not read"
                    )
                for item in marker["retracts"]:
                    if item not in quarantine_ids:
                        raise ParityLedgerError(
                            f"{_describe(marker)} names {item}, which is not a published "
                            "series quarantine record"
                        )
            # Last check: the file each event goes to. Past it, the batch is
            # accepted and its bytes count against those files.
            placement: list[tuple[dict, str]] = []
            planned: dict[str, int] = {}
            current: dict[str, str] = {}
            for event in events:
                family = event_shard(event)
                relative = current.get(family) or last_file(family)
                line = len(canonical_line(event).encode()) + 1
                if ceiling is not None:
                    if line > ceiling:
                        raise ParityLedgerError(
                            f"{_describe(event)} is {line} bytes, over the {ceiling}-byte "
                            "ceiling on one published file"
                        )
                    size = planned.get(relative, size_of(relative))
                    if size + line > ceiling:
                        _, part = shard_part(relative)
                        root, team, host, name = family.rsplit("/", 3)
                        relative = shard_path(root, team, host, name.removesuffix(".jsonl"), part + 1)
                        size = 0
                    planned[relative] = size + line
                current[family] = relative
                placement.append((event, relative))
        except ParityLedgerError as error:
            refused.append({"batch": Path(path).name, "error": str(error)})
            continue
        for relative, size in planned.items():
            sizes[relative] = size
        for event, relative in placement:
            claimed[event["event_id"]] = event
            if relative not in texts:
                texts[relative] = published.texts.get(relative, "")
            texts[relative] += canonical_line(event) + "\n"
            family, part = shard_part(relative)
            latest[family] = relative, part
        appended += len(events)
        fresh.append(path)
    return PublicationPlan(
        texts=texts, appended=appended, fresh_batches=fresh,
        already_published=already, refused=refused,
        base_texts={relative: published.texts.get(relative, "") for relative in texts},
    )


def reapable(path: Path, published: PublishedParity) -> bool:
    """A retained batch may go once the producer root reads every event."""
    try:
        marker, rows = read_batch(path)
    except ParityLedgerError:
        return False
    try:
        if _already_archived([marker, *rows], published.events, published.archived_events):
            return True
    except ParityLedgerError:
        return False
    return all(
        published.events.get(event["event_id"]) is not None
        and same_event(published.events[event["event_id"]], event)
        for event in (marker, *rows)
    )
