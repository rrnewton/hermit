#!/usr/bin/env python3
"""Refuse a hermit-cli test binary that CI does not know exists.

THE DEFECT THIS EXISTS TO REMOVE. Cargo auto-discovers every top-level
``hermit-cli/tests/*.rs`` as a test target, but CI runs them by naming each one
explicitly (``cargo test -p hermit --test <name>`` in ``ci/dag/*.json``). A
binary that is never named is never executed -- and nothing says so. It does not
fail and it does not pass; it is absent from the accounting entirely, so the
gates stay green over it. Measured 2026-08-08 against hermit main 93575493:
118 test targets present, 46 named in the DAG, **72 undeclared**.

THE PRESENT SET IS DERIVED FROM THE TREE, NEVER FROM THE DECLARATIONS FILE.
That is the whole reason this can be trusted: a guard that read its own manifest
to decide what exists would certify itself, and adding a binary without adding a
row would remain invisible -- which is precisely today's bug, one level up. The
declarations file can only ever EXCUSE a binary the tree already proved exists.

TOP LEVEL ONLY. Cargo compiles ``tests/*.rs`` and ``tests/*/main.rs`` as test
targets; ``tests/common/mod.rs`` and friends are shared helper modules and are
NOT targets. An earlier census that stripped directories counted
``tests/common/{liteinst,mod,nondeterminism}.rs`` as three new binaries and
manufactured a "the gap is growing" trend that did not exist. Hence the explicit
depth check rather than a convenient glob.

``none-recorded`` IS AN IOU, NOT AN APPROVAL. The 72 seeded rows say only that
nobody wrote down a reason -- which is true, and is the thing to fix. Marking
them ``intentional`` would have turned 72 problems into 72 approvals and
preserved the blindness while balancing the count. A row may be promoted to a
real disposition WITH a reason at any time; the file can only shrink.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DECLARATIONS = ROOT / "ci" / "undeclared-test-binaries.tsv"
DAGS = ("ci/dag/portable.json", "ci/dag/privileged.json")
# `cargo test -p hermit ...` only. `-p hermit-detcore` names targets in a
# DIFFERENT crate (tests_misc, tests_parallelism); counting those as coverage of
# hermit-cli would excuse a hermit-cli binary on the strength of an unrelated one.
_HERMIT_INVOCATION_RE = re.compile(r"cargo test -p hermit(?!-)[^\"]*")
_TEST_FLAG_RE = re.compile(r"--test\s+([A-Za-z0-9_]+)")
_TOP_LEVEL_TEST_RE = re.compile(r"^hermit-cli/tests/[^/]+\.rs$")


def present_targets() -> set[str]:
    """Cargo test targets that EXIST, read from git rather than from any list."""
    listed = subprocess.run(
        ["git", "-C", str(ROOT), "ls-files", "--", "hermit-cli/tests/"],
        capture_output=True,
        text=True,
        check=False,
    )
    if listed.returncode != 0:
        raise SystemExit(
            f"audit-test-binary-registration: git ls-files failed: "
            f"{listed.stderr.strip()[:200]}"
        )
    names = {
        path.rsplit("/", 1)[-1][: -len(".rs")]
        for path in listed.stdout.splitlines()
        if _TOP_LEVEL_TEST_RE.match(path.strip())
    }
    if not names:
        # Refuse rather than pass: an empty present-set would make every
        # declaration vacuous and the audit silently inert.
        raise SystemExit(
            "audit-test-binary-registration: found NO hermit-cli test targets; "
            "refusing to treat that as a clean audit"
        )
    return names


def registered_targets() -> set[str]:
    """Targets CI actually names, parsed from the committed DAGs."""
    named: set[str] = set()
    for rel in DAGS:
        path = ROOT / rel
        if not path.is_file():
            raise SystemExit(f"audit-test-binary-registration: missing {rel}")
        blob = json.dumps(json.loads(path.read_text()))
        for invocation in _HERMIT_INVOCATION_RE.findall(blob):
            named.update(_TEST_FLAG_RE.findall(invocation))
    return named


def declared() -> dict[str, tuple[str, str]]:
    """name -> (disposition, why), from the versioned declarations file."""
    rows: dict[str, tuple[str, str]] = {}
    if not DECLARATIONS.is_file():
        return rows
    for lineno, raw in enumerate(DECLARATIONS.read_text().splitlines(), 1):
        line = raw.rstrip("\n")
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) < 3 or not parts[0].strip() or not parts[2].strip():
            raise SystemExit(
                f"audit-test-binary-registration: {DECLARATIONS.name}:{lineno} "
                "must be <binary>\\t<disposition>\\t<why>, with a nonempty why"
            )
        rows[parts[0].strip()] = (parts[1].strip(), parts[2].strip())
    return rows


def main() -> int:
    present = present_targets()
    registered = registered_targets()
    rows = declared()

    undeclared = sorted(present - registered - set(rows))
    # A declaration for a binary that no longer exists is stale bookkeeping: it
    # is not a failure, but it must not sit there implying coverage of nothing.
    stale = sorted(set(rows) - present)
    # A row for a binary CI now runs is obsolete in the good direction.
    superseded = sorted(set(rows) & registered)

    excused = sorted(set(rows) & (present - registered))
    none_recorded = [n for n in excused if rows[n][0] == "none-recorded"]

    print(
        f"test-binary registration: {len(present)} target(s) present, "
        f"{len(registered & present)} named in the CI DAG, "
        f"{len(excused)} declared-not-run "
        f"({len(none_recorded)} still `none-recorded`), "
        f"{len(undeclared)} UNDECLARED"
    )
    for name in stale:
        print(f"  NOTE: declaration for {name!r} has no matching test target (stale row)")
    for name in superseded:
        print(f"  NOTE: {name!r} is now run by CI; its declaration can be deleted")

    if undeclared:
        print(
            f"FAIL: {len(undeclared)} hermit-cli test binar"
            f"{'y is' if len(undeclared) == 1 else 'ies are'} neither run by CI nor "
            f"declared:",
            file=sys.stderr,
        )
        for name in undeclared:
            print(f"    hermit-cli/tests/{name}.rs", file=sys.stderr)
        print(
            "\n  A test binary CI never names is never executed, and nothing else "
            "reports that.\n"
            "  Either add it to a `cargo test -p hermit --test <name>` invocation in "
            "ci/dag/*.json,\n"
            f"  or add a row to ci/{DECLARATIONS.name} saying WHY it is not run:\n"
            f"      <name>\\t<disposition>\\t<why>\n"
            "  Use `none-recorded` only if you genuinely do not know; it is an IOU, "
            "not an approval.",
            file=sys.stderr,
        )
        return 2

    print("PASS: every hermit-cli test binary is either run by CI or declared with a reason")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
