#!/usr/bin/env python3
"""Import a qualifying VALIDATE receipt and enforce its scorecard in git log."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

BEGIN = "<!-- COMPATIBILITY-SCORECARD:BEGIN -->"
END = "<!-- COMPATIBILITY-SCORECARD:END -->"
BACKENDS_PATH = "ci/compat/scorecard-backends.json"
RECEIPT_PATH = "ci/compat/commit-scorecard-receipt.json"
E2E_PLAN_PATH = "ci/expected-e2e-plan.json"
BACKEND_ORDER = ["ptrace", "kvm", "liteinst", "e9patch", "sabre", "dbt"]
DISPLAY_ORDER = [*BACKEND_ORDER, "native"]
MANIFEST_INPUTS = [
    "ci/manifest-plan",
    "ci/matrix-symmetry-baseline.json",
    "tests/e2e/manifests",
]
FINAL_ATTRIBUTION = re.compile(
    r"^(?:\[[^\]]+\] \[[^\]]+, devbig[0-9]+\]"
    r"|\[[^,\[\]]+, [^,\[\]]+, [^,\[\]]+, devbig[0-9]+, role=[a-z-]+\])$"
)


class Refusal(RuntimeError):
    pass


def git(*args: str) -> str:
    proc = subprocess.run(["git", *args], text=True, capture_output=True, check=False)
    if proc.returncode:
        raise Refusal(f"git {' '.join(args)} failed: {proc.stderr.strip()}")
    return proc.stdout


class Source:
    def __init__(self, revision: str):
        self.revision = revision

    def read(self, path: str) -> bytes:
        spec = f":{path}" if self.revision == ":" else f"{self.revision}:{path}"
        proc = subprocess.run(["git", "show", spec], capture_output=True, check=False)
        if proc.returncode:
            raise Refusal(f"cannot read {path} from {self.revision}")
        return proc.stdout

    def has(self, path: str) -> bool:
        spec = f":{path}" if self.revision == ":" else f"{self.revision}:{path}"
        return subprocess.run(
            ["git", "cat-file", "-e", spec], capture_output=True, check=False
        ).returncode == 0


@dataclass(frozen=True)
class Scorecard:
    commit: str
    record_id: str
    digest: str
    green: int
    stable_fail: int
    unstable: int
    no_verdict: int
    checks: int
    drop_reason: str
    backend_green: dict[str, int] | None = None
    backend_declared: dict[str, int] | None = None
    backend_ci: dict[str, int] | None = None
    backend_disabled_with_reason: dict[str, int] | None = None
    backend_disabled_without_reason: dict[str, int] | None = None
    plan_digest: str | None = None
    format_version: int = 1

    @property
    def cells(self) -> int:
        if self.backend_green is not None:
            return sum(self.backend_green.values())
        return self.green + self.stable_fail + self.unstable + self.no_verdict


def parse_object(raw: bytes | str, path: str) -> dict:
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise Refusal(f"{path} is not valid JSON: {error}") from error
    if not isinstance(value, dict):
        raise Refusal(f"{path} must contain a JSON object")
    return value


def parse_array(raw: bytes | str, path: str) -> list:
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise Refusal(f"{path} is not valid JSON: {error}") from error
    if not isinstance(value, list):
        raise Refusal(f"{path} must contain a JSON array")
    return value


def validate_receipt_row(
    row: dict,
    digest: str,
    drop_reason: str,
    *,
    backend_green: dict[str, int] | None = None,
    backend_declared: dict[str, int] | None = None,
    backend_ci: dict[str, int] | None = None,
    backend_disabled_with_reason: dict[str, int] | None = None,
    backend_disabled_without_reason: dict[str, int] | None = None,
    plan_digest: str | None = None,
    format_version: int = 1,
) -> Scorecard:
    commit = row.get("commit")
    record_id = row.get("record_id")
    if not isinstance(commit, str) or not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise Refusal("receipt commit must be a full lowercase SHA")
    if not isinstance(record_id, str) or not record_id:
        raise Refusal("receipt record_id must be non-empty")
    if row.get("repo") != "hermit":
        raise Refusal("scorecard source must name the Hermit repository")
    if row.get("producer") != "hermit-validate-rs":
        raise Refusal("scorecard source must be a Hermit VALIDATE receipt")
    if row.get("profile") != "full" or row.get("selection_mode") != "full":
        raise Refusal("scorecard source must be a full-profile, full-selection receipt")
    if row.get("result") != "pass" or row.get("raw_result") != "pass":
        raise Refusal("scorecard source must be a passing receipt")
    if row.get("tree_dirty") is not False or row.get("commit_anchored") is not True:
        raise Refusal("scorecard source must be clean and commit-anchored")

    executed = row.get("executed_tests")
    filtered = row.get("filtered_tests")
    failures = row.get("failures")
    checks = row.get("checks")
    if not isinstance(executed, int) or isinstance(executed, bool) or executed <= 0:
        raise Refusal("receipt executed_tests must be a positive measured count")
    if not isinstance(filtered, int) or isinstance(filtered, bool) or filtered < 0:
        raise Refusal("receipt filtered_tests must be a nonnegative measured count")
    if failures != 0:
        raise Refusal(
            "receipt failures are nonzero, but the receipt does not split them into "
            "STABLE FAIL and UNSTABLE; refusing to invent that classification"
        )
    if not isinstance(checks, int) or isinstance(checks, bool) or checks <= 0:
        raise Refusal("receipt checks must be a positive measured count")
    coverage = row.get("coverage")
    if not isinstance(coverage, dict):
        raise Refusal("receipt coverage is absent")
    if coverage.get("absent_nodes") != [] or coverage.get("zero_executed_nodes") != []:
        raise Refusal("receipt coverage has absent or zero-executed nodes")
    if coverage.get("planned_test_nodes") != coverage.get("executed_test_nodes"):
        raise Refusal("receipt did not execute every planned test node")

    green = sum(backend_green.values()) if backend_green is not None else executed
    no_verdict = 0 if backend_green is not None else filtered
    return Scorecard(
        commit=commit,
        record_id=record_id,
        digest=digest,
        green=green,
        stable_fail=0,
        unstable=0,
        no_verdict=no_verdict,
        checks=checks,
        drop_reason=drop_reason.strip(),
        backend_green=backend_green,
        backend_declared=backend_declared,
        backend_ci=backend_ci,
        backend_disabled_with_reason=backend_disabled_with_reason,
        backend_disabled_without_reason=backend_disabled_without_reason,
        plan_digest=plan_digest,
        format_version=format_version,
    )


def parse_e2e_plan(raw: bytes | str, row: dict) -> dict[str, int]:
    plan = parse_object(raw, E2E_PLAN_PATH)
    cells = plan.get("cells")
    if plan.get("schema") != 1 or not isinstance(cells, list) or not cells:
        raise Refusal(f"{E2E_PLAN_PATH} must be a nonempty schema-1 cell plan")

    counts = {backend: 0 for backend in BACKEND_ORDER}
    identities: set[str] = set()
    expected_gates: set[str] = {"gate.manifest"}
    for index, cell in enumerate(cells):
        if not isinstance(cell, dict):
            raise Refusal(f"{E2E_PLAN_PATH} cell {index} must be an object")
        backend = cell.get("backend")
        lane = cell.get("lane")
        category = cell.get("category")
        if backend not in counts:
            raise Refusal(f"{E2E_PLAN_PATH} cell {index} has unknown backend {backend!r}")
        if lane not in {"portable", "privileged"} or not isinstance(category, str) or not category:
            raise Refusal(f"{E2E_PLAN_PATH} cell {index} has invalid lane/category")
        identity = json.dumps(cell, sort_keys=True, separators=(",", ":"))
        if identity in identities:
            raise Refusal(f"{E2E_PLAN_PATH} contains a duplicate cell at index {index}")
        identities.add(identity)
        counts[backend] += 1
        gate_category = category.replace("-", "_")
        prefix = "e2e" if lane == "portable" else "privileged-e2e"
        expected_gates.add(f"{prefix}.manifest_{gate_category}")

    gates = row.get("gates")
    if not isinstance(gates, list):
        raise Refusal("receipt must carry typed gate results for its E2E plan")
    results: dict[str, list[str]] = {}
    for gate in gates:
        if isinstance(gate, dict) and isinstance(gate.get("name"), str):
            results.setdefault(gate["name"], []).append(gate.get("result"))
    for gate in sorted(expected_gates):
        if results.get(gate) != ["pass"]:
            raise Refusal(f"receipt does not carry exactly one passing {gate} result")
    return counts


def parse_manifest_inventory(
    raw: bytes | str,
) -> tuple[dict[str, int], dict[str, int], dict[str, int], dict[str, int]]:
    cells = parse_array(raw, "manifest inventory")
    if not cells:
        raise Refusal("manifest inventory must contain at least one declared cell")
    declared: dict[str, int] = {}
    ci: dict[str, int] = {}
    disabled_with_reason: dict[str, int] = {}
    disabled_without_reason: dict[str, int] = {}
    identities: set[str] = set()
    for index, cell in enumerate(cells):
        if not isinstance(cell, dict):
            raise Refusal(f"manifest inventory cell {index} must be an object")
        backend = cell.get("backend")
        if not isinstance(backend, str) or not backend:
            raise Refusal(f"manifest inventory cell {index} has no backend")
        if not isinstance(cell.get("ci"), bool):
            raise Refusal(f"manifest inventory cell {index} has no boolean ci field")
        identity = json.dumps(cell, sort_keys=True, separators=(",", ":"))
        if identity in identities:
            raise Refusal(f"manifest inventory contains a duplicate cell at index {index}")
        identities.add(identity)
        declared[backend] = declared.get(backend, 0) + 1
        if cell["ci"]:
            if "ci_disabled_reason" in cell:
                raise Refusal(
                    f"manifest inventory cell {index} is CI-enabled but carries "
                    "ci_disabled_reason"
                )
            ci[backend] = ci.get(backend, 0) + 1
        else:
            ci.setdefault(backend, 0)
            reason = cell.get("ci_disabled_reason")
            if reason is None:
                disabled_without_reason[backend] = (
                    disabled_without_reason.get(backend, 0) + 1
                )
            elif isinstance(reason, str) and reason.strip():
                disabled_with_reason[backend] = disabled_with_reason.get(backend, 0) + 1
            else:
                raise Refusal(
                    f"manifest inventory cell {index} has an empty or invalid "
                    "ci_disabled_reason"
                )
        disabled_with_reason.setdefault(backend, 0)
        disabled_without_reason.setdefault(backend, 0)
    for backend, total in declared.items():
        classified = (
            ci.get(backend, 0)
            + disabled_with_reason.get(backend, 0)
            + disabled_without_reason.get(backend, 0)
        )
        if classified != total:
            raise Refusal(
                f"manifest inventory classification for {backend} covers "
                f"{classified}/{total} cells"
            )
    return declared, ci, disabled_with_reason, disabled_without_reason


def enrich_manifest_inventory(plan_raw: str, harness_raw: str) -> str:
    """Join expanded cells to their whole-mode CI exclusion reason.

    Both inputs come from hermit-manifest-plan at the receipt commit.  The normal
    JSON format is authoritative for expanded backend cells; harness-json keeps
    the source mode's optional ci_disabled_reason.  Joining them here avoids a
    second implementation of manifest expansion while preserving the distinction
    between a recorded reason and an absent one.
    """

    cells = parse_array(plan_raw, "manifest inventory")
    documents = parse_array(harness_raw, "manifest harness")
    metadata: dict[tuple[str, str, str, str], tuple[bool, bool, object]] = {}
    for document_index, document in enumerate(documents):
        if not isinstance(document, dict):
            raise Refusal(f"manifest harness document {document_index} must be an object")
        bucket = document.get("bucket")
        tests = document.get("test")
        if not isinstance(bucket, str) or not bucket or not isinstance(tests, list):
            raise Refusal(
                f"manifest harness document {document_index} has invalid bucket/tests"
            )
        for test_index, test in enumerate(tests):
            if not isinstance(test, dict):
                raise Refusal(
                    f"manifest harness document {document_index} test {test_index} "
                    "must be an object"
                )
            test_id = test.get("id")
            modes = test.get("modes")
            if not isinstance(test_id, str) or not test_id or not isinstance(modes, dict):
                raise Refusal(
                    f"manifest harness document {document_index} test {test_index} "
                    "has invalid id/modes"
                )
            for mode, spec in modes.items():
                if not isinstance(mode, str) or not mode or not isinstance(spec, dict):
                    raise Refusal(f"manifest harness {test_id} has invalid mode metadata")
                enabled = spec.get("backends_enabled")
                ci = spec.get("ci")
                if not isinstance(enabled, list) or not isinstance(ci, bool):
                    raise Refusal(
                        f"manifest harness {test_id}/{mode} has invalid enabled/ci metadata"
                    )
                reason_present = "ci_disabled_reason" in spec
                reason = spec.get("ci_disabled_reason")
                for backend in enabled:
                    if not isinstance(backend, str) or not backend:
                        raise Refusal(
                            f"manifest harness {test_id}/{mode} has an invalid backend"
                        )
                    key = (bucket, test_id, mode, backend)
                    if key in metadata:
                        raise Refusal(f"manifest harness duplicates expanded cell {key!r}")
                    metadata[key] = (ci, reason_present, reason)

    enriched: list[dict] = []
    seen: set[tuple[str, str, str, str]] = set()
    for index, cell in enumerate(cells):
        if not isinstance(cell, dict):
            raise Refusal(f"manifest inventory cell {index} must be an object")
        key_values = tuple(cell.get(field) for field in ("bucket", "test", "mode", "backend"))
        if not all(isinstance(value, str) and value for value in key_values):
            raise Refusal(f"manifest inventory cell {index} has an invalid identity")
        key = key_values
        if key in seen:
            raise Refusal(f"manifest inventory duplicates expanded cell {key!r}")
        seen.add(key)
        if key not in metadata:
            raise Refusal(f"manifest inventory cell {key!r} has no harness metadata")
        ci, reason_present, reason = metadata[key]
        if cell.get("ci") is not ci:
            raise Refusal(f"manifest inventory and harness disagree on ci for {key!r}")
        copy = dict(cell)
        if reason_present:
            copy["ci_disabled_reason"] = reason
        enriched.append(copy)

    extra = sorted(set(metadata) - seen)
    if extra:
        raise Refusal(
            f"manifest harness has {len(extra)} enabled backend cells absent from inventory; "
            f"first={extra[0]!r}"
        )
    canonical = json.dumps(enriched, sort_keys=True, separators=(",", ":"))
    parse_manifest_inventory(canonical)
    return canonical


def read_manifest_inventory(commit: str) -> str:
    head = git("rev-parse", "HEAD").strip()
    changed = git("diff", "--name-only", commit, head, "--", *MANIFEST_INPUTS).splitlines()
    dirty = git("status", "--porcelain=v1", "--", *MANIFEST_INPUTS).splitlines()
    if changed or dirty:
        raise Refusal(
            "cannot bind manifest inventory to receipt commit: manifest inputs differ "
            f"(committed={changed}, dirty={dirty})"
        )
    outputs: dict[str, str] = {}
    for output_format in ("json", "harness-json"):
        proc = subprocess.run(
            [
                "cargo",
                "run",
                "--quiet",
                "-p",
                "hermit-manifest-plan",
                "--",
                "--format",
                output_format,
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        if proc.returncode:
            raise Refusal(
                f"manifest inventory {output_format} command failed: "
                f"{proc.stderr.strip()}"
            )
        outputs[output_format] = proc.stdout.strip()
    return enrich_manifest_inventory(outputs["json"], outputs["harness-json"])


def load_scorecard(source: Source) -> Scorecard:
    backends = parse_object(source.read(BACKENDS_PATH), BACKENDS_PATH).get("backends")
    if backends != BACKEND_ORDER:
        raise Refusal(f"{BACKENDS_PATH} must preserve the owner-specified backend order")
    wrapper = parse_object(source.read(RECEIPT_PATH), RECEIPT_PATH)
    schema = wrapper.get("schema")
    if schema not in {1, 2, 3, 4, 5}:
        raise Refusal(f"{RECEIPT_PATH} schema must be 1, 2, 3, 4, or 5")
    canonical = wrapper.get("canonical_receipt")
    digest = wrapper.get("receipt_sha256")
    drop_reason = wrapper.get("drop_reason", "")
    if not isinstance(canonical, str) or not canonical:
        raise Refusal(f"{RECEIPT_PATH} canonical_receipt must be a non-empty string")
    if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise Refusal(f"{RECEIPT_PATH} receipt_sha256 must be a lowercase SHA-256")
    actual = hashlib.sha256(canonical.encode()).hexdigest()
    if actual != digest:
        raise Refusal(f"{RECEIPT_PATH} canonical receipt digest mismatch")
    if not isinstance(drop_reason, str):
        raise Refusal(f"{RECEIPT_PATH} drop_reason must be a string")
    row = parse_object(canonical, "canonical receipt")
    if schema == 1:
        # Read-only compatibility for the first, superseded aggregate commit.
        # Read-only compatibility. New imports write the latest schema.
        return validate_receipt_row(row, digest, drop_reason)

    canonical_plan = wrapper.get("canonical_e2e_plan")
    plan_digest = wrapper.get("e2e_plan_sha256")
    if not isinstance(canonical_plan, str) or not canonical_plan:
        raise Refusal(f"{RECEIPT_PATH} canonical_e2e_plan must be a non-empty string")
    if not isinstance(plan_digest, str) or not re.fullmatch(r"[0-9a-f]{64}", plan_digest):
        raise Refusal(f"{RECEIPT_PATH} e2e_plan_sha256 must be a lowercase SHA-256")
    if hashlib.sha256(canonical_plan.encode()).hexdigest() != plan_digest:
        raise Refusal(f"{RECEIPT_PATH} E2E plan digest mismatch")
    commit_plan = Source(row.get("commit", "")).read(E2E_PLAN_PATH).decode()
    if commit_plan != canonical_plan:
        raise Refusal("stored E2E plan does not equal the plan at the receipt commit")
    backend_green = parse_e2e_plan(canonical_plan, row)
    backend_declared = None
    backend_ci = None
    backend_disabled_with_reason = None
    backend_disabled_without_reason = None
    if schema >= 4:
        canonical_inventory = wrapper.get("canonical_manifest_inventory")
        inventory_digest = wrapper.get("manifest_inventory_sha256")
        if not isinstance(canonical_inventory, str) or not canonical_inventory:
            raise Refusal(f"{RECEIPT_PATH} canonical_manifest_inventory must be non-empty")
        if not isinstance(inventory_digest, str) or not re.fullmatch(
            r"[0-9a-f]{64}", inventory_digest
        ):
            raise Refusal(f"{RECEIPT_PATH} manifest_inventory_sha256 must be a lowercase SHA-256")
        if hashlib.sha256(canonical_inventory.encode()).hexdigest() != inventory_digest:
            raise Refusal(f"{RECEIPT_PATH} manifest inventory digest mismatch")
        (
            backend_declared,
            backend_ci,
            parsed_with_reason,
            parsed_without_reason,
        ) = parse_manifest_inventory(canonical_inventory)
        if schema >= 5:
            backend_disabled_with_reason = parsed_with_reason
            backend_disabled_without_reason = parsed_without_reason
        for backend, selected in backend_green.items():
            if selected > backend_ci.get(backend, 0):
                raise Refusal(
                    f"selected {backend} cells exceed the manifest's declared CI cells"
                )
    return validate_receipt_row(
        row,
        digest,
        drop_reason,
        backend_green=backend_green,
        backend_declared=backend_declared,
        backend_ci=backend_ci,
        backend_disabled_with_reason=backend_disabled_with_reason,
        backend_disabled_without_reason=backend_disabled_without_reason,
        plan_digest=plan_digest,
        format_version=schema,
    )


def signed(value: int) -> str:
    return f"{value:+d}"


def render(current: Scorecard, previous: Scorecard | None) -> str:
    comparable = bool(
        previous
        and current.backend_green is not None
        and previous.backend_green is not None
    )
    green_change = signed(current.green - previous.green) if comparable else "BASELINE"
    matrix_change = signed(current.cells - previous.cells) if comparable else "BASELINE"
    if comparable and current.green < previous.green and not current.drop_reason:
        raise Refusal("GREEN decreased but drop_reason is empty")
    lines = [BEGIN, "Compatibility scorecard (source: qualifying VALIDATE receipt)"]
    if current.backend_green is None:
        lines.extend(
            [
                "| backend | receipt sha | GREEN | STABLE FAIL | UNSTABLE | NO VERDICT | cells |",
                "|---|---|---:|---:|---:|---:|---:|",
            ]
        )
        for backend in BACKEND_ORDER:
            lines.append(f"| {backend} | {current.digest} | — | — | — | — | — |")
    elif current.format_version == 2:
        lines.extend(
            [
                "| backend | receipt sha | GREEN | STABLE FAIL | UNSTABLE | NO VERDICT | cells |",
                "|---|---|---:|---:|---:|---:|---:|",
            ]
        )
        for backend in BACKEND_ORDER:
            green = current.backend_green[backend]
            lines.append(f"| {backend} | {current.digest} | {green} | 0 | 0 | 0 | {green} |")
    elif current.format_version == 3:
        lines.extend(
            [
                "| backend | receipt sha | measurement | GREEN | STABLE FAIL | UNSTABLE | NO VERDICT | cells |",
                "|---|---|---|---:|---:|---:|---:|---:|",
            ]
        )
        for backend in BACKEND_ORDER:
            green = current.backend_green[backend]
            if green == 0:
                lines.append(
                    f"| {backend} | {current.digest} | UNMEASURED | — | — | — | — | 0 |"
                )
            else:
                lines.append(
                    f"| {backend} | {current.digest} | MEASURED | {green} | 0 | 0 | 0 | {green} |"
                )
    elif current.format_version == 4:
        if current.backend_declared is None or current.backend_ci is None:
            raise Refusal("schema-4 scorecard is missing manifest declaration counts")
        lines.extend(
            [
                "| backend | receipt sha | selection | declared cells | declared CI cells | "
                "GREEN | STABLE FAIL | UNSTABLE | NO VERDICT | selected cells |",
                "|---|---|---|---:|---:|---:|---:|---:|---:|---:|",
            ]
        )
        actual_backends = set(current.backend_declared)
        for backend in DISPLAY_ORDER:
            if backend not in actual_backends:
                lines.append(
                    f"| {backend} | {current.digest} | NOT A BACKEND | — | — | — | — | — | — | — |"
                )
                continue
            declared = current.backend_declared[backend]
            declared_ci = current.backend_ci.get(backend, 0)
            selected = current.backend_green.get(backend, 0)
            if selected == 0:
                lines.append(
                    f"| {backend} | {current.digest} | DECLARED BUT NOT SELECTED | "
                    f"{declared} | {declared_ci} | — | — | — | — | 0 |"
                )
            else:
                lines.append(
                    f"| {backend} | {current.digest} | SELECTED | {declared} | "
                    f"{declared_ci} | {selected} | 0 | 0 | 0 | {selected} |"
                )
    else:
        if (
            current.backend_declared is None
            or current.backend_ci is None
            or current.backend_disabled_with_reason is None
            or current.backend_disabled_without_reason is None
        ):
            raise Refusal("schema-5 scorecard is missing whole-cell reason counts")
        lines.extend(
            [
                "| backend | receipt sha | selection | declared cells | declared CI cells | "
                "DISABLED WITH REASON | DISABLED WITH NO REASON RECORDED | GREEN | "
                "STABLE FAIL | UNSTABLE | NO VERDICT | selected cells |",
                "|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
            ]
        )
        actual_backends = set(current.backend_declared)
        for backend in DISPLAY_ORDER:
            if backend not in actual_backends:
                lines.append(
                    f"| {backend} | {current.digest} | NOT A BACKEND | — | — | — | — | "
                    "— | — | — | — | — |"
                )
                continue
            declared = current.backend_declared[backend]
            declared_ci = current.backend_ci.get(backend, 0)
            with_reason = current.backend_disabled_with_reason.get(backend, 0)
            without_reason = current.backend_disabled_without_reason.get(backend, 0)
            selected = current.backend_green.get(backend, 0)
            if selected == 0:
                lines.append(
                    f"| {backend} | {current.digest} | DECLARED BUT NOT SELECTED | "
                    f"{declared} | {declared_ci} | {with_reason} | {without_reason} | "
                    "— | — | — | — | 0 |"
                )
            else:
                lines.append(
                    f"| {backend} | {current.digest} | SELECTED | {declared} | "
                    f"{declared_ci} | {with_reason} | {without_reason} | {selected} | "
                    f"0 | 0 | 0 | {selected} |"
                )
    if current.backend_green is None or current.format_version == 2:
        lines.append(
            f"| TOTAL | {current.digest} | {current.green} | {current.stable_fail} | "
            f"{current.unstable} | {current.no_verdict} | {current.cells} |"
        )
    elif current.format_version == 3:
        lines.append(
            f"| TOTAL | {current.digest} | MEASURED | {current.green} | "
            f"{current.stable_fail} | {current.unstable} | {current.no_verdict} | "
            f"{current.cells} |"
        )
    elif current.format_version == 4:
        assert current.backend_declared is not None and current.backend_ci is not None
        lines.append(
            f"| TOTAL | {current.digest} | SELECTED | "
            f"{sum(current.backend_declared.values())} | {sum(current.backend_ci.values())} | "
            f"{current.green} | {current.stable_fail} | {current.unstable} | "
            f"{current.no_verdict} | {current.cells} |"
        )
    else:
        assert current.backend_declared is not None and current.backend_ci is not None
        assert current.backend_disabled_with_reason is not None
        assert current.backend_disabled_without_reason is not None
        lines.append(
            f"| TOTAL | {current.digest} | SELECTED | "
            f"{sum(current.backend_declared.values())} | {sum(current.backend_ci.values())} | "
            f"{sum(current.backend_disabled_with_reason.values())} | "
            f"{sum(current.backend_disabled_without_reason.values())} | "
            f"{current.green} | {current.stable_fail} | {current.unstable} | "
            f"{current.no_verdict} | {current.cells} |"
        )
    lines.append(
        f"Receipt: commit {current.commit}; record {current.record_id}; checks {current.checks}."
    )
    if current.backend_green is None:
        lines.append(
            "Backend split: unavailable in this receipt; no counts were inferred from gate names."
        )
    elif current.format_version == 2:
        lines.append(
            f"E2E plan: {current.plan_digest}; backend rows and TOTAL are derived from "
            "the plan at the receipt commit."
        )
    elif current.format_version == 3:
        unmeasured = [
            backend for backend in BACKEND_ORDER if current.backend_green[backend] == 0
        ]
        measured = len(BACKEND_ORDER) - len(unmeasured)
        lines.append(
            f"E2E plan: {current.plan_digest}; backend rows and TOTAL are derived from "
            "the plan at the receipt commit."
        )
        lines.append(
            f"Backend measurement: {measured}/{len(BACKEND_ORDER)} measured; "
            f"unmeasured: {', '.join(unmeasured) if unmeasured else 'none'}."
        )
    else:
        assert current.backend_declared is not None
        actual_backends = set(current.backend_declared)
        declared_not_selected = [
            backend
            for backend in DISPLAY_ORDER
            if backend in actual_backends and current.backend_green.get(backend, 0) == 0
        ]
        not_backends = [backend for backend in BACKEND_ORDER if backend not in actual_backends]
        lines.append(
            f"E2E plan: {current.plan_digest}; GREEN and selected-cell TOTAL are "
            "derived from the plan at the receipt commit."
        )
        lines.append(
            "Declared but not selected: "
            f"{', '.join(declared_not_selected) if declared_not_selected else 'none'}; "
            f"not a backend: {', '.join(not_backends) if not_backends else 'none'}."
        )
        if current.format_version >= 5:
            assert current.backend_disabled_with_reason is not None
            assert current.backend_disabled_without_reason is not None
            lines.append(
                "Whole-cell CI exclusions: "
                f"{sum(current.backend_disabled_with_reason.values())} disabled with reason; "
                f"{sum(current.backend_disabled_without_reason.values())} disabled with no "
                "reason recorded."
            )
    lines.append(
        f"Matrix change: {matrix_change}; GREEN change: {green_change}. "
        "Matrix growth is reported separately and is not a GREEN regression."
    )
    if previous and not comparable:
        lines.append("Comparison: source transition; no delta is claimed against the legacy aggregate.")
    if comparable and current.green < previous.green:
        lines.append(f"GREEN DROP: {green_change}; reason: {current.drop_reason}")
    else:
        lines.append("GREEN DROP: none.")
    lines.append(END)
    return "\n".join(lines)


def optional_scorecard(revision: str) -> Scorecard | None:
    source = Source(revision)
    return load_scorecard(source) if source.has(RECEIPT_PATH) else None


def expected_for(revision: str) -> str:
    current = load_scorecard(Source(revision))
    try:
        parent = git("rev-parse", f"{revision}^").strip()
    except Refusal:
        parent = ""
    return render(current, optional_scorecard(parent) if parent else None)


def replace_block(message: str, block: str) -> str:
    if BEGIN in message or END in message:
        if message.count(BEGIN) != 1 or message.count(END) != 1:
            raise Refusal("commit message has malformed compatibility scorecard markers")
        start = message.index(BEGIN)
        end = message.index(END, start) + len(END)
        message = message[:start].rstrip() + "\n\n" + message[end:].lstrip()
    lines = message.rstrip().splitlines()
    tail_start = len(lines)
    last = next((i for i in range(len(lines) - 1, -1, -1) if lines[i].strip()), -1)
    if last >= 0 and FINAL_ATTRIBUTION.fullmatch(lines[last].strip()):
        tail_start = last
        while tail_start > 0 and (
            not lines[tail_start - 1].strip()
            or lines[tail_start - 1].strip().startswith("Task: ")
        ):
            tail_start -= 1
    if tail_start == len(lines):
        return "\n".join(lines).rstrip() + "\n\n" + block + "\n"
    head = "\n".join(lines[:tail_start]).rstrip()
    tail = "\n".join(lines[tail_start:]).lstrip("\n")
    return head + "\n\n" + block + "\n\n" + tail + "\n"


def check_message(message: str, expected: str) -> None:
    if message.count(BEGIN) != 1 or message.count(END) != 1:
        raise Refusal("commit message must contain exactly one compatibility scorecard")
    start = message.index(BEGIN)
    end = message.index(END, start) + len(END)
    if message[start:end] != expected:
        raise Refusal("commit message compatibility scorecard is missing, stale, or altered")


def check_commit(revision: str) -> None:
    revision = git("rev-parse", revision).strip()
    check_message(git("show", "-s", "--format=%B", revision), expected_for(revision))


def check_range(base: str, head: str) -> None:
    base_sha = git("rev-parse", base).strip()
    head_sha = git("rev-parse", head).strip()
    merge_base = git("merge-base", base_sha, head_sha).strip()
    commits = git("rev-list", "--reverse", f"{merge_base}..{head_sha}").splitlines()
    if not commits:
        commits = [head_sha]
    for commit in commits:
        check_commit(commit)
    print(f"PASS: {len(commits)}/{len(commits)} commit scorecards match their receipt")


def import_receipt(output: Path, drop_reason: str) -> None:
    canonical = sys.stdin.read().strip()
    digest = hashlib.sha256(canonical.encode()).hexdigest()
    row = parse_object(canonical, "canonical receipt")
    commit = row.get("commit")
    if not isinstance(commit, str) or not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise Refusal("receipt commit must be a full lowercase SHA")
    canonical_plan = Source(commit).read(E2E_PLAN_PATH).decode()
    plan_digest = hashlib.sha256(canonical_plan.encode()).hexdigest()
    backend_green = parse_e2e_plan(canonical_plan, row)
    canonical_inventory = read_manifest_inventory(commit)
    inventory_digest = hashlib.sha256(canonical_inventory.encode()).hexdigest()
    (
        backend_declared,
        backend_ci,
        backend_disabled_with_reason,
        backend_disabled_without_reason,
    ) = parse_manifest_inventory(canonical_inventory)
    for backend, selected in backend_green.items():
        if selected > backend_ci.get(backend, 0):
            raise Refusal(f"selected {backend} cells exceed the manifest's declared CI cells")
    validate_receipt_row(
        row,
        digest,
        drop_reason,
        backend_green=backend_green,
        backend_declared=backend_declared,
        backend_ci=backend_ci,
        backend_disabled_with_reason=backend_disabled_with_reason,
        backend_disabled_without_reason=backend_disabled_without_reason,
        plan_digest=plan_digest,
    )
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(
            {
                "schema": 5,
                "receipt_sha256": digest,
                "canonical_receipt": canonical,
                "e2e_plan_sha256": plan_digest,
                "canonical_e2e_plan": canonical_plan,
                "manifest_inventory_sha256": inventory_digest,
                "canonical_manifest_inventory": canonical_inventory,
                "drop_reason": drop_reason,
            },
            indent=2,
        )
        + "\n"
    )
    print(f"imported qualifying receipt sha256={digest}")


def check_scorecard_only_child(validated_parent: str, candidate: str) -> None:
    parent = git("rev-parse", validated_parent).strip()
    child = git("rev-parse", candidate).strip()
    if git("rev-parse", f"{child}^").strip() != parent:
        raise Refusal("candidate is not exactly one child of the validated parent")
    if git("diff", "--name-only", parent, child).splitlines() != [RECEIPT_PATH]:
        raise Refusal(f"scorecard-only child must change only {RECEIPT_PATH}")
    if load_scorecard(Source(child)).commit != parent:
        raise Refusal("scorecard-only child receipt must describe its exact validated parent")
    check_commit(child)
    # TODO(scorecard-table-in-every-commit): replace this narrow scorecard-only
    # exception with the general local VALIDATE documentation-only fast path,
    # sharing the existing GitHub CI path classifier rather than duplicating it.
    print(f"PASS: scorecard-only child {child} inherits exact-parent green {parent}")


def main() -> int:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("render")
    insert = commands.add_parser("insert")
    insert.add_argument("message_file")
    insert.add_argument(
        "--previous",
        default="HEAD",
        help="revision whose scorecard precedes the commit being prepared",
    )
    check = commands.add_parser("check-message")
    check.add_argument("message_file")
    check.add_argument(
        "--previous",
        default="HEAD",
        help="revision whose scorecard precedes the commit being checked",
    )
    commit = commands.add_parser("check-commit")
    commit.add_argument("revision")
    range_parser = commands.add_parser("check-range")
    range_parser.add_argument("--base", required=True)
    range_parser.add_argument("--head", default="HEAD")
    importer = commands.add_parser("import-receipt")
    importer.add_argument("--output", default=RECEIPT_PATH)
    importer.add_argument("--drop-reason", default="")
    inherit = commands.add_parser("check-scorecard-only-child")
    inherit.add_argument("--validated-parent", required=True)
    inherit.add_argument("--candidate", required=True)
    args = parser.parse_args()

    if args.command in {"render", "insert", "check-message"}:
        previous = args.previous if args.command in {"insert", "check-message"} else "HEAD"
        expected = render(load_scorecard(Source(":")), optional_scorecard(previous))
        if args.command == "render":
            print(expected)
        elif args.command == "insert":
            path = Path(args.message_file)
            path.write_text(replace_block(path.read_text(), expected))
        else:
            check_message(Path(args.message_file).read_text(), expected)
    elif args.command == "check-commit":
        check_commit(args.revision)
    elif args.command == "check-range":
        check_range(args.base, args.head)
    elif args.command == "import-receipt":
        import_receipt(Path(args.output), args.drop_reason)
    else:
        check_scorecard_only_child(args.validated_parent, args.candidate)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Refusal as error:
        print(f"COMMIT SCORECARD REFUSED: {error}", file=sys.stderr)
        raise SystemExit(1)
