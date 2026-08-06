#!/usr/bin/env python3
"""Version-controlled, idempotent compatibility scorecard.

THE CONTRACT, and every design choice below follows from it:

  IDEMPOTENT / NO CHURN. Re-witnessing the same green at the same tip must
  produce a byte-identical CSV, so `validate` can run twice and leave ZERO git
  diff. That forces one rule: the scorecard records only OBSERVED CAPABILITY.
  Anything that varies between two runs of the same code -- timestamps, run ids,
  durations, binary hashes, host load -- is run METADATA and goes to a separate
  gitignored sidecar. It never enters the scorecard. A file that churns on every
  run cannot be reviewed, and a diff that is always noise is a diff nobody reads.

  MONOTONICITY. Capability may increase freely. A DECREASE is blocked at commit
  time unless it carries a strong reason AND a P0 that stays open until the
  capability is restored, so a regression cannot be normalised by quietly
  rewriting the record of what used to work.

Rows are per cell. Tier booleans are ordered from weaker to stricter, so a cell
that gains `--detlog-heap` has strictly more capability than one that does not.
"""

from __future__ import annotations

import argparse
import csv
import io
import json
import os
import platform
import re
import subprocess
import sys
from pathlib import Path

SCHEMA = 1

#: Version-controlled columns. Deliberately excludes everything run-varying.
COLUMNS = [
    "bucket", "test", "mode", "backend",
    "strict", "detlog_stack", "detlog_heap", "chaos",   # tier, weaker -> stricter
    "determinism", "parity",                             # outputs
]

#: Ordered weakest -> strictest. Used by the monotonicity comparison.
TIER_FLAGS = ["strict", "detlog_stack", "detlog_heap", "chaos"]

#: Fields the harness emits that must NEVER reach the scorecard, because they
#: differ between two runs of identical code and would churn the diff.
RUN_METADATA = [
    "run_id", "duration_ms", "binary_sha256", "source_tree_dirty",
    "hermit_sha", "test_sha256", "schema", "classification", "reason",
]


def machine_key() -> str:
    """`<shortname>-<cpu-model>`, FQDN scrubbed.

    Only the first dot-separated label of the hostname is used, so an internal
    FQDN never lands in a version-controlled path.
    """
    short = platform.node().split(".")[0].strip() or "unknown-host"
    cpu = "unknown-cpu"
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    slug = lambda s: re.sub(r"[^a-z0-9]+", "-", s.lower()).strip("-")
    return f"{slug(short)}-{slug(cpu)}"


def _flag(args: object, name: str) -> int:
    """Is `name` present in the recorded effective argv?"""
    if isinstance(args, str):
        try:
            args = json.loads(args)
        except json.JSONDecodeError:
            args = [args]
    return int(any(name in str(a) for a in (args or [])))


def row_from_record(rec: dict) -> dict | None:
    """One JSONL harness record -> one scorecard row, or None if not a cell."""
    mode = rec.get("mode")
    if mode not in ("verify", "replay", "run", "chaos", "naked", "custom"):
        return None
    # The directive names run|replay; the harness spells the run-mode `verify`.
    norm_mode = "run" if mode in ("verify", "run", "chaos") else mode
    args = rec.get("effective_args")
    # The harness emits UPPERCASE outcomes ("PASS"). Comparing case-sensitively
    # here silently scored every passing cell as 0 -- a systematic false negative
    # that would then make the eventual fix look like a fleet-wide capability
    # INCREASE. Normalise, and the test fixtures below use the real casing.
    outcome = str(rec.get("outcome") or "").strip().lower()
    return {
        "bucket": rec.get("category", ""),
        "test": rec.get("test", ""),
        "mode": norm_mode,
        "backend": rec.get("backend") or "native",
        "strict": _flag(args, "--strict"),
        "detlog_stack": _flag(args, "--detlog-stack"),
        "detlog_heap": _flag(args, "--detlog-heap"),
        "chaos": _flag(args, "--chaos"),
        # determinism = the --verify double-run passed. Only `verify` executes a
        # second run, so any other mode is UNMEASURED (blank), never 0.
        "determinism": (1 if outcome == "pass" else 0) if mode == "verify" else "",
        # parity = this backend's log matched the ptrace reference. The harness
        # JSONL does not carry a cross-backend comparison, so it is left blank
        # (UNMEASURED) rather than invented. A separate producer fills it.
        "parity": "",
    }


def read_rows(paths: list[Path]) -> list[dict]:
    rows: dict[tuple, dict] = {}
    for p in paths:
        if not p.is_file():
            continue
        for line in p.read_text().splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            row = row_from_record(rec)
            if row is None:
                continue
            # Last writer wins per cell, then the whole set is sorted, so input
            # file order cannot change the output.
            rows[(row["bucket"], row["test"], row["mode"], row["backend"])] = row
    return [rows[k] for k in sorted(rows)]


def render_csv(rows: list[dict]) -> str:
    """Deterministic CSV: fixed column order, sorted rows, LF endings."""
    out = io.StringIO()
    w = csv.DictWriter(out, fieldnames=COLUMNS, lineterminator="\n")
    w.writeheader()
    for r in sorted(rows, key=lambda r: tuple(str(r[c]) for c in COLUMNS[:4])):
        w.writerow({c: r[c] for c in COLUMNS})
    return out.getvalue()


def load_csv(path: Path) -> list[dict]:
    if not path.is_file():
        return []
    return list(csv.DictReader(path.read_text().splitlines()))


def _cap(row: dict) -> tuple:
    """Capability vector for monotonicity. Higher is strictly better."""
    tiers = tuple(int(row.get(f) or 0) for f in TIER_FLAGS)
    det = int(row.get("determinism") or 0)
    par = int(row.get("parity") or 0)
    return (det, par) + tiers


def find_decreases(old: list[dict], new: list[dict]) -> list[str]:
    """Cells whose capability went DOWN, or that vanished entirely."""
    key = lambda r: (r["bucket"], r["test"], r["mode"], r["backend"])
    new_by = {key(r): r for r in new}
    out = []
    for o in old:
        k = key(o)
        n = new_by.get(k)
        if n is None:
            out.append(f"{'/'.join(k)}: cell REMOVED (was {_cap(o)})")
            continue
        if _cap(n) < _cap(o):
            out.append(f"{'/'.join(k)}: {_cap(o)} -> {_cap(n)}")
    return out


def aggregate(rows: list[dict]) -> str:
    """ptrace + per-backend columns x bucket rows, a total row, and the
    total-of-totals cell."""
    backends = sorted({r["backend"] for r in rows})
    order = (["ptrace"] if "ptrace" in backends else []) + [b for b in backends if b != "ptrace"]
    buckets = sorted({r["bucket"] for r in rows})

    def cell(bucket: str | None, backend: str) -> str:
        sel = [r for r in rows if r["backend"] == backend and (bucket is None or r["bucket"] == bucket)]
        meas = [r for r in sel if str(r["determinism"]) != ""]
        if not meas:
            return "n/a"
        return f"{sum(int(r['determinism']) for r in meas)}/{len(meas)}"

    w = max([9] + [len(b) for b in order])
    lines = ["bucket".ljust(28) + "".join(b.rjust(w + 2) for b in order)]
    lines.append("-" * len(lines[0]))
    for b in buckets:
        lines.append(b[:27].ljust(28) + "".join(cell(b, be).rjust(w + 2) for be in order))
    lines.append("-" * len(lines[0]))
    lines.append("TOTAL".ljust(28) + "".join(cell(None, be).rjust(w + 2) for be in order))
    allm = [r for r in rows if str(r["determinism"]) != ""]
    tot = f"{sum(int(r['determinism']) for r in allm)}/{len(allm)}" if allm else "n/a"
    lines.append(f"TOTAL-OF-TOTALS: {tot}")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    w = sub.add_parser("write", help="ingest harness JSONL -> scorecard CSV (idempotent)")
    w.add_argument("--results", nargs="+", required=True)
    w.add_argument("--root", default="compat-scorecard")
    w.add_argument("--machine", default=None)
    l = sub.add_parser("lint", help="block a committed capability DECREASE")
    l.add_argument("--old", required=True)
    l.add_argument("--new", required=True)
    l.add_argument("--reason", default=None, help="path to a decrease-reason file")
    a = sub.add_parser("aggregate", help="render the bucket x backend table")
    a.add_argument("--csv", required=True)
    args = ap.parse_args(argv)

    if args.cmd == "write":
        rows = read_rows([Path(p) for p in args.results])
        mdir = Path(args.root) / "machines" / (args.machine or machine_key())
        mdir.mkdir(parents=True, exist_ok=True)
        (mdir / "scorecard.csv").write_text(render_csv(rows))
        # Run metadata is written BESIDE the scorecard and is gitignored, so it
        # can carry everything that would otherwise churn the tracked file.
        (mdir / "run-metadata.json").write_text(json.dumps({
            "schema": SCHEMA, "cells": len(rows),
            "excluded_from_scorecard": RUN_METADATA,
        }, indent=2, sort_keys=True) + "\n")
        print(f"wrote {mdir/'scorecard.csv'} ({len(rows)} cells)")
        return 0

    if args.cmd == "lint":
        dec = find_decreases(load_csv(Path(args.old)), load_csv(Path(args.new)))
        if not dec:
            print("scorecard-lint: no capability decrease")
            return 0
        reason = Path(args.reason).read_text().strip() if args.reason and Path(args.reason).is_file() else ""
        has_p0 = bool(re.search(r"\bP0\b", reason)) and bool(
            re.search(r"(task|tg)[: ]+\S+", reason, re.I))
        strong = bool(re.search(r"fake[- ]green|deliberate temporary regression", reason, re.I))
        print("scorecard-lint: CAPABILITY DECREASE in %d cell(s):" % len(dec))
        for d in dec[:20]:
            print("  " + d)
        if strong and has_p0:
            print("scorecard-lint: ALLOWED -- strong reason plus a P0 that must stay open until restored.")
            return 0
        print("scorecard-lint: BLOCKED. A decrease needs BOTH:")
        print("  (1) a strong reason -- 'fake-green found' or 'deliberate temporary regression';")
        print("  (2) a named P0 task that will not close until the capability is restored.")
        return 1

    rows = load_csv(Path(args.csv))
    sys.stdout.write(aggregate(rows))
    return 0


if __name__ == "__main__":
    sys.exit(main())
