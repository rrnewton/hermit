#!/usr/bin/env python3
"""Render and verify the per-commit compatibility scorecard.

The matrix denominator is derived from the schema-v2 e2e manifests and the
tracked backend list. No cell count is stored in policy or in the result file.
Cells not named by strict evidence remain NO VERDICT; absence never becomes a
pass or a failure.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

BEGIN = "<!-- COMPATIBILITY-SCORECARD:BEGIN -->"
END = "<!-- COMPATIBILITY-SCORECARD:END -->"
BACKENDS_PATH = "ci/compat/scorecard-backends.json"
RESULTS_PATH = "ci/compat/commit-scorecard-results.json"
MANIFEST_PREFIX = "tests/e2e/manifests/"
EXPECTED_BACKEND_ORDER = ["ptrace", "kvm", "liteinst", "e9patch", "sabre", "dbt"]
STATE_KEYS = ("green", "stable_fail", "unstable")
FINAL_ATTRIBUTION = re.compile(r"^\[[^\]]+\] \[[^\]]+, devbig[0-9]+\]$")


class Refusal(RuntimeError):
    pass


def git(*args: str, input_text: str | None = None) -> str:
    proc = subprocess.run(
        ["git", *args], input=input_text, text=True, capture_output=True, check=False
    )
    if proc.returncode:
        raise Refusal(f"git {' '.join(args)} failed: {proc.stderr.strip()}")
    return proc.stdout


class Source:
    def __init__(self, revision: str):
        self.revision = revision

    def paths(self, prefix: str) -> list[str]:
        if self.revision == ":":
            output = git("ls-files", "--", prefix)
        else:
            output = git("ls-tree", "-r", "--name-only", self.revision, "--", prefix)
        return sorted(line for line in output.splitlines() if line)

    def read(self, path: str) -> bytes:
        spec = f":{path}" if self.revision == ":" else f"{self.revision}:{path}"
        proc = subprocess.run(["git", "show", spec], capture_output=True, check=False)
        if proc.returncode:
            raise Refusal(f"cannot read {path} from {self.revision}: {proc.stderr.decode().strip()}")
        return proc.stdout

    def has(self, path: str) -> bool:
        spec = f":{path}" if self.revision == ":" else f"{self.revision}:{path}"
        return subprocess.run(
            ["git", "cat-file", "-e", spec], capture_output=True, check=False
        ).returncode == 0


@dataclass(frozen=True)
class Population:
    population_id: str
    standard_id: str
    source_sha256: str
    programs: frozenset[str]
    backends: tuple[str, ...]

    @property
    def cells(self) -> int:
        return len(self.programs) * len(self.backends)


@dataclass(frozen=True)
class Row:
    backend: str
    green: int
    stable_fail: int
    unstable: int
    no_verdict: int

    @property
    def cells(self) -> int:
        return self.green + self.stable_fail + self.unstable + self.no_verdict


@dataclass(frozen=True)
class Scorecard:
    population: Population
    rows: tuple[Row, ...]
    drop_reason: str

    @property
    def green(self) -> int:
        return sum(row.green for row in self.rows)


def decode_json(raw: bytes, path: str) -> dict:
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise Refusal(f"{path} is not valid JSON: {error}") from error
    if not isinstance(value, dict):
        raise Refusal(f"{path} must contain a JSON object")
    return value


def load_population(source: Source) -> Population:
    config_raw = source.read(BACKENDS_PATH)
    config = decode_json(config_raw, BACKENDS_PATH)
    backends = config.get("backends")
    if backends != EXPECTED_BACKEND_ORDER:
        raise Refusal(
            f"{BACKENDS_PATH} backends must be ordered {' '.join(EXPECTED_BACKEND_ORDER)}"
        )
    population_id = config.get("population_id")
    standard_id = config.get("standard_id")
    if not isinstance(population_id, str) or not population_id:
        raise Refusal(f"{BACKENDS_PATH} needs a non-empty population_id")
    if not isinstance(standard_id, str) or not standard_id:
        raise Refusal(f"{BACKENDS_PATH} needs a non-empty standard_id")

    manifest_paths = [
        path
        for path in source.paths(MANIFEST_PREFIX)
        if path.endswith(".toml") and "/inventory/" not in path
    ]
    if not manifest_paths:
        raise Refusal(f"no schema-v2 manifests found under {MANIFEST_PREFIX}")

    programs: set[str] = set()
    digest = hashlib.sha256()
    for path in [BACKENDS_PATH, *manifest_paths]:
        raw = source.read(path)
        digest.update(path.encode())
        digest.update(b"\0")
        digest.update(raw)
        digest.update(b"\0")
        if path == BACKENDS_PATH:
            continue
        try:
            manifest = tomllib.loads(raw.decode())
        except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
            raise Refusal(f"{path} is not valid TOML: {error}") from error
        if manifest.get("schema") != 2:
            raise Refusal(f"{path} must use schema 2")
        tests = manifest.get("test")
        if not isinstance(tests, list) or not tests:
            raise Refusal(f"{path} needs a non-empty [[test]] array")
        for test in tests:
            test_id = test.get("id") if isinstance(test, dict) else None
            if not isinstance(test_id, str) or not test_id:
                raise Refusal(f"{path} has a test without a non-empty id")
            if test_id in programs:
                raise Refusal(f"duplicate corpus program id: {test_id}")
            programs.add(test_id)

    return Population(
        population_id=population_id,
        standard_id=standard_id,
        source_sha256=digest.hexdigest(),
        programs=frozenset(programs),
        backends=tuple(backends),
    )


def load_scorecard(source: Source) -> Scorecard:
    population = load_population(source)
    results = decode_json(source.read(RESULTS_PATH), RESULTS_PATH)
    if results.get("schema") != 1:
        raise Refusal(f"{RESULTS_PATH} schema must be 1")
    if results.get("population_id") != population.population_id:
        raise Refusal(
            f"{RESULTS_PATH} population_id does not name the tracked corpus definition"
        )
    if results.get("standard_id") != population.standard_id:
        raise Refusal(f"{RESULTS_PATH} standard_id does not name the tracked standard")
    states = results.get("states")
    if not isinstance(states, dict) or list(states) != list(population.backends):
        raise Refusal(f"{RESULTS_PATH} states must follow the tracked backend order")

    rows: list[Row] = []
    for backend in population.backends:
        backend_states = states.get(backend)
        if not isinstance(backend_states, dict) or set(backend_states) != set(STATE_KEYS):
            raise Refusal(f"{RESULTS_PATH} {backend} must contain {', '.join(STATE_KEYS)}")
        assigned: set[str] = set()
        counts: dict[str, int] = {}
        for state in STATE_KEYS:
            ids = backend_states[state]
            if not isinstance(ids, list) or any(not isinstance(item, str) for item in ids):
                raise Refusal(f"{RESULTS_PATH} {backend}.{state} must be a string array")
            if len(ids) != len(set(ids)):
                raise Refusal(f"{RESULTS_PATH} {backend}.{state} contains duplicate ids")
            unknown = set(ids) - population.programs
            if unknown:
                raise Refusal(
                    f"{RESULTS_PATH} {backend}.{state} names ids outside the corpus: "
                    + ", ".join(sorted(unknown))
                )
            overlap = assigned & set(ids)
            if overlap:
                raise Refusal(
                    f"{RESULTS_PATH} {backend} assigns multiple states to: "
                    + ", ".join(sorted(overlap))
                )
            assigned.update(ids)
            counts[state] = len(ids)
        rows.append(
            Row(
                backend=backend,
                green=counts["green"],
                stable_fail=counts["stable_fail"],
                unstable=counts["unstable"],
                no_verdict=len(population.programs - assigned),
            )
        )
    drop_reason = results.get("drop_reason", "")
    if not isinstance(drop_reason, str):
        raise Refusal(f"{RESULTS_PATH} drop_reason must be a string")
    return Scorecard(population, tuple(rows), drop_reason.strip())


def signed(value: int) -> str:
    return f"{value:+d}"


def render(current: Scorecard, previous: Scorecard | None) -> str:
    compatible = bool(
        previous
        and previous.population.population_id == current.population.population_id
        and previous.population.standard_id == current.population.standard_id
    )
    previous_rows = {row.backend: row for row in previous.rows} if compatible else {}
    lines = [
        BEGIN,
        "Compatibility scorecard",
        "| backend | population | standard | source sha256 | GREEN | STABLE FAIL | UNSTABLE | NO VERDICT | cells | GREEN change | matrix change |",
        "|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for row in current.rows:
        old = previous_rows.get(row.backend)
        green_change = signed(row.green - old.green) if old else "BASELINE"
        matrix_change = signed(row.cells - old.cells) if old else "BASELINE"
        lines.append(
            f"| {row.backend} | {current.population.population_id} | "
            f"{current.population.standard_id} | {current.population.source_sha256} | "
            f"{row.green} | {row.stable_fail} | {row.unstable} | {row.no_verdict} | "
            f"{row.cells} | {green_change} | {matrix_change} |"
        )
    total_green = current.green
    old_green = previous.green if compatible and previous else 0
    old_cells = previous.population.cells if compatible and previous else 0
    lines.append(
        f"| TOTAL | {current.population.population_id} | {current.population.standard_id} | "
        f"{current.population.source_sha256} | {total_green} | "
        f"{sum(r.stable_fail for r in current.rows)} | {sum(r.unstable for r in current.rows)} | "
        f"{sum(r.no_verdict for r in current.rows)} | {current.population.cells} | "
        f"{signed(total_green - old_green) if compatible else 'BASELINE'} | "
        f"{signed(current.population.cells - old_cells) if compatible else 'BASELINE'} |"
    )
    if compatible and previous:
        program_change = len(current.population.programs) - len(previous.population.programs)
        lines.append(
            "Matrix size: "
            f"{len(current.population.programs)} programs x {len(current.population.backends)} backends "
            f"= {current.population.cells} cells; program change {signed(program_change)}, "
            f"cell change {signed(current.population.cells - previous.population.cells)}."
        )
        green_change = current.green - previous.green
        if green_change < 0:
            if not current.drop_reason:
                raise Refusal("GREEN decreased but drop_reason is empty")
            lines.append(f"GREEN DROP: {signed(green_change)}; reason: {current.drop_reason}")
        else:
            lines.append(f"GREEN change: {signed(green_change)}; no drop.")
    else:
        lines.append(
            "Matrix size: "
            f"{len(current.population.programs)} programs x {len(current.population.backends)} backends "
            f"= {current.population.cells} cells; baseline (not compared with another population)."
        )
        lines.append("GREEN change: BASELINE; no cross-population delta.")
    lines.append(END)
    return "\n".join(lines)


def parent_scorecard(revision: str) -> Scorecard | None:
    try:
        parent = git("rev-parse", f"{revision}^").strip()
    except Refusal:
        return None
    source = Source(parent)
    return load_scorecard(source) if source.has(RESULTS_PATH) else None


def optional_scorecard(revision: str) -> Scorecard | None:
    source = Source(revision)
    return load_scorecard(source) if source.has(RESULTS_PATH) else None


def expected_for(source_revision: str, parent_revision: str | None = None) -> str:
    current = load_scorecard(Source(source_revision))
    previous = load_scorecard(Source(parent_revision)) if parent_revision else parent_scorecard(source_revision)
    return render(current, previous)


def replace_block(message: str, block: str) -> str:
    if BEGIN in message or END in message:
        if message.count(BEGIN) != 1 or message.count(END) != 1:
            raise Refusal("commit message has malformed compatibility scorecard markers")
        start = message.index(BEGIN)
        end = message.index(END, start) + len(END)
        message = message[:start].rstrip() + "\n\n" + message[end:].lstrip()
    lines = message.rstrip().splitlines()
    tail_start = len(lines)
    last_nonempty = next(
        (index for index in range(len(lines) - 1, -1, -1) if lines[index].strip()), -1
    )
    if last_nonempty >= 0 and FINAL_ATTRIBUTION.fullmatch(lines[last_nonempty].strip()):
        tail_start = last_nonempty
        while tail_start > 0:
            prior = lines[tail_start - 1].strip()
            if not prior or prior.startswith("Task: "):
                tail_start -= 1
            else:
                break
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
    actual = message[start:end]
    if actual != expected:
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
    print(f"PASS: {len(commits)}/{len(commits)} commit scorecards match their exact corpus source")


def check_scorecard_only_child(validated_parent: str, candidate: str) -> None:
    parent = git("rev-parse", validated_parent).strip()
    child = git("rev-parse", candidate).strip()
    actual_parent = git("rev-parse", f"{child}^").strip()
    if actual_parent != parent:
        raise Refusal("candidate is not exactly one child of the validated parent")
    changed = git("diff", "--name-only", parent, child).splitlines()
    if changed != [RESULTS_PATH]:
        raise Refusal(f"scorecard-only child must change only {RESULTS_PATH}")
    results = decode_json(Source(child).read(RESULTS_PATH), RESULTS_PATH)
    if results.get("measured_hermit_sha") != parent:
        raise Refusal("scorecard-only child must name its exact validated parent as measured_hermit_sha")
    check_commit(child)
    # TODO(scorecard-table-in-every-commit): replace this narrow scorecard-only
    # exception with the general local VALIDATE documentation-only fast path,
    # sharing the existing GitHub CI path classifier rather than duplicating it.
    print(f"PASS: scorecard-only child {child} inherits exact-parent green {parent}")


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("render")
    insert = sub.add_parser("insert")
    insert.add_argument("message_file")
    check = sub.add_parser("check-message")
    check.add_argument("message_file")
    commit = sub.add_parser("check-commit")
    commit.add_argument("revision")
    range_parser = sub.add_parser("check-range")
    range_parser.add_argument("--base", required=True)
    range_parser.add_argument("--head", default="HEAD")
    inherit = sub.add_parser("check-scorecard-only-child")
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
    else:
        check_scorecard_only_child(args.validated_parent, args.candidate)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Refusal as error:
        print(f"COMMIT SCORECARD REFUSED: {error}", file=sys.stderr)
        raise SystemExit(1)
