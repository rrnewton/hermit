#!/usr/bin/env python3
"""Bracket validate.sh's artifact purge: it must PURGE the artifact and KEEP the record.

The scan's stated purpose is to make build-artifact corruption measurable. Before
this test existed it recorded a bare count, so the frequency was measurable and no
individual occurrence was diagnosable -- which is the failure this brackets against.

Both sides are planted, because a detector that deletes everything and a detector
that records things it did not delete both look "green" against negatives alone:

  NEGATIVE  one artifact per corruption predicate -> must be purged AND recorded.
  POSITIVE  a healthy artifact of every inspected format, plus one with an
            uninspected extension -> must survive AND never appear in the evidence.

Then it asserts the retained record answers, without the artifact: WHICH FILE,
PRODUCED BY WHAT, WHEN it was written, and UNDER WHICH RUN.

Fixtures are synthesized here (no dependency on a warm target/ tree), so the ELF
and ar headers under test are exactly the shapes the classifier claims to read.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
VALIDATE = ROOT / "validate.sh"

# The two functions under test, sliced out of validate.sh so this exercises the
# REAL implementation rather than a copy. If either marker moves the slice comes
# back empty and every assertion below fails loudly -- it cannot silently pass.
_SLICE = (
    "/^function artifact_purge_evidence_path/,/^}$/p;"
    "/^function purge_zero_byte_objects/,/^}$/p"
)

FAILURES: list[str] = []


def check(ok: bool, what: str) -> None:
    print(f"  {'PASS' if ok else 'FAIL'}  {what}")
    if not ok:
        FAILURES.append(what)


def elf64(shoff: int, shentsize: int, shnum: int, size: int) -> bytes:
    """A 64-byte ELF64 header padded to `size`. The section table fits iff
    shoff + shentsize*shnum <= size, which is exactly the classifier's predicate."""
    head = bytearray(64)
    head[0:8] = b"\x7fELF\x02\x01\x01\x00"
    struct.pack_into("<H", head, 0x10, 1)  # e_type = ET_REL
    struct.pack_into("<Q", head, 0x28, shoff)
    struct.pack_into("<H", head, 0x3A, shentsize)
    struct.pack_into("<H", head, 0x3C, shnum)
    return bytes(head) + b"\x00" * max(0, size - 64)


def ar(members_bytes: int) -> bytes:
    return b"!<arch>\n" + b"x" * members_bytes


def write(path: Path, data: bytes, mtime: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    subprocess.run(["touch", "-d", mtime, str(path)], check=True)
    return path


def run_scan(root: Path, parent: Path, *, enabled: bool, cap: str = "") -> tuple[str, Path, dict]:
    """Source the sliced functions and invoke the scan exactly as validate.sh does."""
    evidence = parent / "ignored" / "validate-artifact-purge.jsonl"
    summary = parent / "summary.json"
    env = os.environ.copy()
    env.update(
        ROOT_DIR=str(root),
        DEV_HERMIT_PARENT=str(parent),
        VALIDATION_COMMIT="deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        VALIDATION_TREE="cafebabecafebabecafebabecafebabecafebabe",
        VALIDATION_HOST="test-host",
        VALIDATION_SLOT="test-slot",
        VALIDATION_PROFILE="full",
        VALIDATION_CACHE_STATE="warm",
        VALIDATION_STARTED_AT="2026-08-07T17:00:00Z",
        VALIDATION_STARTED_EPOCH="1786122000",
    )
    if enabled:
        env["VALIDATE_PURGE_ARTIFACTS"] = "1"
    else:
        env.pop("VALIDATE_PURGE_ARTIFACTS", None)
    if cap:
        env["VALIDATE_PURGE_MAX_RECORDS"] = cap
    script = f'''
set -uo pipefail
source <(sed -n '{_SLICE}' "{VALIDATE}")
purge_zero_byte_objects "$ROOT_DIR/target" "{evidence}" "{summary}"
'''
    out = subprocess.run(["bash", "-c", script], env=env, capture_output=True, text=True)
    if out.returncode != 0:
        raise AssertionError(f"scan failed rc={out.returncode}: {out.stderr}")
    records = {}
    if evidence.is_file():
        records = [json.loads(line) for line in evidence.read_text().splitlines() if line]
    else:
        records = []
    return out.stdout.strip(), evidence, records


def main() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="hermit-purge-evidence-"))
    root, parent = tmp / "checkout", tmp / "parent"
    (parent / "ignored").mkdir(parents=True)
    t = root / "target"

    # ---------------- NEGATIVES: one per corruption predicate ----------------
    # Paths mirror the real producer shapes so the inferred producer is exercised:
    # a build-script out/ tree, a cmake target dir, cargo deps, cargo incremental.
    negatives = {
        "zero_size": write(
            t / "release/build/reverie-dbi-9c9420178ef00fb4/out/dynamorio-build/lib64/release/libdynamorio_static.o",
            b"", "2026-08-05 03:14:15 UTC"),
        "elf_section_table_truncated": write(
            t / "install-build/dbi-client/CMakeFiles/detcore_dbi_link_stub.dir/detcore_dbi_link_stub.c.o",
            elf64(shoff=4096, shentsize=64, shnum=10, size=256), "2026-08-06 22:01:02 UTC"),
        "elf_bad_magic": write(
            t / "debug/deps/libfoo-1111111111111111.o",
            b"cc1plus: fatal error: Killed signal terminated program\n", "2026-08-04 09:00:00 UTC"),
        "elf_header_short": write(
            t / "debug/incremental/hermit_detcore-3333333333333333/s-abc-def/xyz.o",
            b"\x7fELF\x02\x01\x01\x00short", "2026-08-07 01:00:00 UTC"),
        "ar_bad_magic": write(
            t / "debug/deps/libbar-2222222222222222.rlib",
            b"not-an-archive" + b"p" * 80, "2026-08-03 12:30:00 UTC"),
        "ar_too_short": write(
            t / "release/build/heapless-94c299068f90bf50/out/libprobe.a",
            ar(10), "2026-08-06 18:45:00 UTC"),
    }
    # ---------------- POSITIVES: must survive and never be recorded ----------
    positives = [
        write(t / "debug/deps/good-4444444444444444.o",
              elf64(shoff=64, shentsize=64, shnum=1, size=256), "2026-08-06 10:00:00 UTC"),
        write(t / "debug/deps/libgood-5555555555555555.rlib", ar(200), "2026-08-06 10:00:00 UTC"),
        write(t / "release/build/ok-6666666666666666/out/libok.a", ar(200), "2026-08-06 10:00:00 UTC"),
        # Not an inspected extension: must never be touched or recorded.
        write(t / "debug/deps/notinspected-7777777777777777.d", b"garbage\n", "2026-08-06 10:00:00 UTC"),
    ]

    print(f"planted {len(negatives)} corrupt + {len(positives)} healthy artifacts")
    count, evidence, records = run_scan(root, parent, enabled=True)
    by_path = {r["path"]: r for r in records}

    print("\n== NEGATIVE bracket: purged AND recorded, with the right reason ==")
    check(count == str(len(negatives)), f"scan stdout is a bare {len(negatives)} (got {count!r})")
    for reason, path in negatives.items():
        rel = str(path.relative_to(root))
        check(not path.exists(), f"purged: {rel}")
        rec = by_path.get(rel)
        check(rec is not None, f"recorded: {rel}")
        if rec:
            check(rec["reason"] == reason, f"reason is {reason} (got {rec['reason']})")

    print("\n== POSITIVE bracket: healthy artifacts survive and are not recorded ==")
    for path in positives:
        rel = str(path.relative_to(root))
        check(path.exists(), f"survived: {rel}")
        check(rel not in by_path, f"not recorded: {rel}")

    print("\n== the record answers the four questions WITHOUT the artifact ==")
    for rel, rec in sorted(by_path.items()):
        prod = rec["producer"]
        chain = " -> ".join(
            c.get("target") or c.get("package") or c.get("crate") or c["kind"]
            for c in prod["chain"])
        check(bool(rec["path"]), f"WHICH FILE: {rel}")
        check(chain != "unattributed" and prod["inferred_from"] == "path",
              f"PRODUCED BY WHAT: {chain} (inferred from path)")
        check(rec["mtime"] is not None and rec["age_seconds"] is not None,
              f"WHEN: mtime {rec['mtime']}, age {rec['age_seconds']}s")
        run = rec["run"]
        check(run["commit"].startswith("deadbeef") and run["host"] == "test-host"
              and run["profile"] == "full" and run["cache_state"] == "warm",
              f"UNDER WHICH RUN: {run['commit'][:8]}@{run['host']} ({run['profile']},{run['cache_state']})")
        check(rec["size_bytes"] is not None and rec["head_hex"] is not None,
              f"plus size={rec['size_bytes']} head={rec['head_hex'][:16] or '(empty)'}")
    # The standing hypothesis is that corruption lives in unfingerprinted
    # build-script/cmake output, so the record must state that per artifact
    # rather than leaving it to be re-derived by hand from the path.
    residents = [r for r in records if r["producer"]["cargo_tracked"] is False]
    check(len(residents) == 3,
          f"cargo_tracked=false marks the 3 non-cargo residents (got {len(residents)})")

    print("\n== DISABLED default stays inert ==")
    tmp2 = Path(tempfile.mkdtemp(prefix="hermit-purge-off-"))
    r2, p2 = tmp2 / "checkout", tmp2 / "parent"
    (p2 / "ignored").mkdir(parents=True)
    victim = write(r2 / "target/debug/deps/zero-8888888888888888.o", b"", "2026-08-06 10:00:00 UTC")
    c2, ev2, rec2 = run_scan(r2, p2, enabled=False)
    check(c2 == "0", f"returns 0 when disabled (got {c2!r})")
    check(victim.exists(), "corrupt artifact NOT purged when disabled")
    check(not rec2, "no evidence written when disabled")

    print("\n== the record cap is enforced but never silent ==")
    tmp3 = Path(tempfile.mkdtemp(prefix="hermit-purge-cap-"))
    r3, p3 = tmp3 / "checkout", tmp3 / "parent"
    (p3 / "ignored").mkdir(parents=True)
    for i in range(10):
        write(r3 / f"target/debug/deps/z{i}-99999999999999{i:02d}.o", b"", "2026-08-06 10:00:00 UTC")
    c3, ev3, rec3 = run_scan(r3, p3, enabled=True, cap="3")
    summary = json.loads((p3 / "summary.json").read_text())
    check(c3 == "10", f"all 10 purged: the COUNT is not capped (got {c3!r})")
    check(len(rec3) == 3, f"evidence capped at 3 records (got {len(rec3)})")
    check(summary["purged"] == 10 and summary["recorded"] == 3 and summary["truncated"] is True,
          "summary states purged=10 recorded=3 truncated=true, so the cap is visible")

    print()
    if FAILURES:
        print(f"FAILED ({len(FAILURES)}):")
        for f in FAILURES:
            print(f"  - {f}")
        sys.exit(1)
    print("artifact-purge evidence: all brackets pass")


if __name__ == "__main__":
    main()
