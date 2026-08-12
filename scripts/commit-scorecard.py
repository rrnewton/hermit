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
FINAL_ATTRIBUTION = re.compile(r"^\[[^\]]+\] \[[^\]]+, devbig[0-9]+\]$")


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
    plan_digest: str | None = None

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


def validate_receipt_row(
    row: dict,
    digest: str,
    drop_reason: str,
    *,
    backend_green: dict[str, int] | None = None,
    plan_digest: str | None = None,
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
        plan_digest=plan_digest,
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


def load_scorecard(source: Source) -> Scorecard:
    backends = parse_object(source.read(BACKENDS_PATH), BACKENDS_PATH).get("backends")
    if backends != BACKEND_ORDER:
        raise Refusal(f"{BACKENDS_PATH} must preserve the owner-specified backend order")
    wrapper = parse_object(source.read(RECEIPT_PATH), RECEIPT_PATH)
    schema = wrapper.get("schema")
    if schema not in {1, 2}:
        raise Refusal(f"{RECEIPT_PATH} schema must be 1 or 2")
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
        # New imports always write schema 2 and must carry the receipt-bound plan.
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
    return validate_receipt_row(
        row,
        digest,
        drop_reason,
        backend_green=backend_green,
        plan_digest=plan_digest,
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
    lines = [
        BEGIN,
        "Compatibility scorecard (source: qualifying VALIDATE receipt)",
        "| backend | receipt sha | GREEN | STABLE FAIL | UNSTABLE | NO VERDICT | cells |",
        "|---|---|---:|---:|---:|---:|---:|",
    ]
    if current.backend_green is None:
        for backend in BACKEND_ORDER:
            lines.append(f"| {backend} | {current.digest} | — | — | — | — | — |")
    else:
        for backend in BACKEND_ORDER:
            green = current.backend_green[backend]
            lines.append(f"| {backend} | {current.digest} | {green} | 0 | 0 | 0 | {green} |")
    lines.append(
        f"| TOTAL | {current.digest} | {current.green} | {current.stable_fail} | "
        f"{current.unstable} | {current.no_verdict} | {current.cells} |"
    )
    lines.append(
        f"Receipt: commit {current.commit}; record {current.record_id}; checks {current.checks}."
    )
    if current.backend_green is None:
        lines.append(
            "Backend split: unavailable in this receipt; no counts were inferred from gate names."
        )
    else:
        lines.append(
            f"E2E plan: {current.plan_digest}; backend rows and TOTAL are derived from "
            "the plan at the receipt commit."
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
    validate_receipt_row(
        row,
        digest,
        drop_reason,
        backend_green=backend_green,
        plan_digest=plan_digest,
    )
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(
            {
                "schema": 2,
                "receipt_sha256": digest,
                "canonical_receipt": canonical,
                "e2e_plan_sha256": plan_digest,
                "canonical_e2e_plan": canonical_plan,
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
    check = commands.add_parser("check-message")
    check.add_argument("message_file")
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
        expected = render(load_scorecard(Source(":")), optional_scorecard("HEAD"))
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
