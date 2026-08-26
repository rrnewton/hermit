#!/usr/bin/env python3
"""Refuse a refusal-classification predicate that matches a marker unanchored.

⚠️ THE CHANNEL CARRIES MORE THAN THE PRODUCER WRITES. A predicate that decides
"did the child DECLINE, or did it FAIL?" by asking whether a marker appears
anywhere in a captured log is verified against what the producing program emits
and then run against a channel that other writers also use -- wrappers, prefixes,
nested tooling, and the log's own quoted prose. A MENTION of the marker is then
read as an ASSERTION of it.

The concrete instance this exists for: `scripts/test_validate_stop_paths.py`
classified a child validate with

    return any(shape in output for shape in _REFUSAL_SHAPES)

`output` is the whole captured log. Any line that merely QUOTES "refused by:" --
including the diagnostic text this same suite prints, and including a nested
tool echoing a command -- makes a genuine crash classify as a could-not-evaluate.
That direction is the dangerous one: a crash reported as "declined to run" is
silent, and silence is what the change containing this predicate was written to
remove.

⚠️ WHY A CHECKER AND NOT A COMMENT. The protection today is prose on two pull
request threads and a task note. This project has already established what a
convention with no mechanism does: it does not stop work, it randomises it --
the conservative lander strands and the permissive one lands, and neither
outcome is correctable afterwards. A comment is read by whoever happens to read
it; this refuses.

WHAT IS REQUIRED INSTEAD. Match the marker ANCHORED to the start of a line:

    return any(
        line.strip().startswith(shape)
        for line in output.splitlines()
        for shape in SHAPES
    )

or a regex with `^\s*` under `re.MULTILINE`. Anchoring is what distinguishes the
producing program's own line-initial output from a quotation of it inside
somebody else's line.

⚠️ STRIP BEFORE ANCHORING. This file previously recommended a bare
`line.startswith(shape)` and that advice was WRONG. Measured by
`agent(hermit-101)` on hermit#2699: `RunSummary::refused` puts its reasons in
`detail` and the renderer indents every detail line by three spaces
(`scripts/validate.rs:11068`), so of the three real shapes only
"validate: REFUSED" is at column zero. A bare `startswith` would have stopped
recognising TWO OF THREE genuine declines -- converting a false positive into a
false negative, which is the worse direction. The checker cannot tell the two
forms apart (both are anchored), so the only defence is that this text names the
right one.

⚠️ THIS IS PYTHON, NOT rust-script, AND THAT IS DELIBERATE. `AGENTS.md` prefers
rust-script for new scripts. rust-script compiles, and this was written under an
explicit no-box-time instruction on a box at load 41. Porting it to rust-script
is a reasonable follow-up and needs no behaviour change; the detection rule is
twelve lines of AST walk.

EXIT STATUS
    0  no unanchored refusal predicate found
    1  at least one found -- each named with file, line, and the offending call
    2  usage error / could not parse a file it was asked to check
"""

from __future__ import annotations

import ast
import sys
from pathlib import Path

# A collection whose NAME says it holds refusal/verdict markers. Keyed on the
# name rather than the contents: the contents are string literals that will
# legitimately differ per call site, and it is the ROLE that makes an unanchored
# match wrong.
MARKER_NAME_HINTS = ("REFUS", "VERDICT", "DECLINE")

# ⚠️ AND THE HAYSTACK MUST PLAUSIBLY BE A CAPTURED CHANNEL. Without this the rule
# fires on any membership test against a collection whose name merely contains one
# of the hints -- including, measured on its first run, THIS FILE's own
# `hint in upper` name test. That is a name comparison, not a classification of
# another program's output, and flagging it would have made the checker red on a
# clean main, which is worse than not having it.
CHANNEL_NAME_HINTS = (
    "output", "log", "stdout", "stderr", "captured", "seen", "text", "content", "body", "blob",
)


def _is_channel(node: ast.AST) -> bool:
    """Does this expression plausibly hold a captured channel rather than a word?"""
    if isinstance(node, ast.Name):
        return any(h in node.id.lower() for h in CHANNEL_NAME_HINTS)
    if isinstance(node, ast.Attribute):
        return any(h in node.attr.lower() for h in CHANNEL_NAME_HINTS)
    if isinstance(node, ast.Call):
        return _is_channel(node.func)
    return False


def _is_marker_name(name: str) -> bool:
    upper = name.upper()
    return any(hint in upper for hint in MARKER_NAME_HINTS)


def _marker_collections(tree: ast.Module) -> set[str]:
    """Module-level names bound to a tuple/list/set of string literals."""
    found: set[str] = set()
    for node in ast.walk(tree):
        if not isinstance(node, ast.Assign):
            continue
        if not isinstance(node.value, (ast.Tuple, ast.List, ast.Set)):
            continue
        elts = node.value.elts
        if not elts or not all(
            isinstance(e, ast.Constant) and isinstance(e.value, str) for e in elts
        ):
            continue
        for target in node.targets:
            if isinstance(target, ast.Name) and _is_marker_name(target.id):
                found.add(target.id)
    return found


def _line_bound_names(fn: ast.AST) -> set[str]:
    """Names that provably hold ONE LINE, so `x in line` is already anchored enough.

    A comprehension over `.splitlines()` binds its target to a single line. This
    is what the corrected form looks like, and it must not be flagged.
    """
    bound: set[str] = set()
    for node in ast.walk(fn):
        if not isinstance(node, (ast.comprehension,)):
            continue
        it = node.iter
        if (
            isinstance(it, ast.Call)
            and isinstance(it.func, ast.Attribute)
            and it.func.attr == "splitlines"
            and isinstance(node.target, ast.Name)
        ):
            bound.add(node.target.id)
    return bound


def offences(path: Path) -> list[tuple[int, str]]:
    try:
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    except SyntaxError as error:
        raise SystemExit(f"check-refusal-predicate-anchored: cannot parse {path}: {error}")
    markers = _marker_collections(tree)
    if not markers:
        return []
    per_line = _line_bound_names(tree)
    out: list[tuple[int, str]] = []
    for node in ast.walk(tree):
        # `<needle> in <haystack>` where the needle comes from a marker collection
        # and the haystack is NOT a single line.
        if not isinstance(node, ast.Compare) or len(node.ops) != 1:
            continue
        if not isinstance(node.ops[0], ast.In):
            continue
        haystack = node.comparators[0]
        if isinstance(haystack, ast.Name) and haystack.id in per_line:
            continue  # anchored: the haystack is one line
        if isinstance(haystack, ast.Call) and isinstance(haystack.func, ast.Attribute):
            if haystack.func.attr == "splitlines":
                continue
        needle = node.left
        # direct: `SHAPE_CONST in blob` via a comprehension target over markers
        names = {n.id for n in ast.walk(node) if isinstance(n, ast.Name)}
        if names & markers or (isinstance(needle, ast.Name) and needle.id in per_line):
            out.append((node.lineno, ast.unparse(node)))
            continue
        # `any(shape in blob for shape in MARKERS)` -- the marker name is on the
        # comprehension's iterable, not inside the Compare, so look outward.
    for node in ast.walk(tree):
        if not isinstance(node, (ast.GeneratorExp, ast.ListComp, ast.SetComp)):
            continue
        iters = {
            n.id
            for gen in node.generators
            if isinstance(gen.iter, ast.Name)
            for n in [gen.iter]
        }
        if not (iters & markers):
            continue
        targets = {
            gen.target.id for gen in node.generators if isinstance(gen.target, ast.Name)
        }
        for inner in ast.walk(node.elt):
            if (
                isinstance(inner, ast.Compare)
                and len(inner.ops) == 1
                and isinstance(inner.ops[0], ast.In)
                and isinstance(inner.left, ast.Name)
                and inner.left.id in targets
            ):
                hay = inner.comparators[0]
                if isinstance(hay, ast.Name) and hay.id in per_line:
                    continue
                if not _is_channel(hay):
                    continue
                out.append((inner.lineno, ast.unparse(node)))
    return sorted(set(out))


def main(argv: list[str]) -> int:
    args = argv[1:]
    if args and args[0] == "--self-test":
        return self_test()
    root = Path(args[0]) if args else Path(".")
    targets = sorted(root.rglob("*.py")) if root.is_dir() else [root]
    bad = 0
    for path in targets:
        if "/.git/" in str(path):
            continue
        for lineno, src in offences(path):
            bad += 1
            print(
                f"{path}:{lineno}: refusal marker matched UNANCHORED against a whole "
                f"channel: {src}",
                file=sys.stderr,
            )
    if bad:
        print(
            "\ncheck-refusal-predicate-anchored: REFUSED -- "
            f"{bad} unanchored refusal predicate(s).\n"
            "  The captured channel carries more than the producing program writes, so a\n"
            "  MENTION of the marker is read as an ASSERTION and a crash classifies as a\n"
            "  decline. Match the marker anchored to the start of a line:\n"
            "      any(line.startswith(m) for line in output.splitlines() for m in MARKERS)\n"
            "  or a `^`-anchored regex under re.MULTILINE.",
            file=sys.stderr,
        )
        return 1
    print("check-refusal-predicate-anchored: OK -- no unanchored refusal predicate.")
    return 0


UNANCHORED = '''
_REFUSAL_SHAPES = ("refused by:", "validate: REFUSED")
def looks_refused(output):
    return any(shape in output for shape in _REFUSAL_SHAPES)
'''

ANCHORED = '''
_REFUSAL_SHAPES = ("refused by:", "validate: REFUSED")
def looks_refused(output):
    return any(
        line.strip().startswith(shape)
        for line in output.splitlines()
        for shape in _REFUSAL_SHAPES
    )
'''

# ⚠️ ACCEPTED, AND THE CHECKER CANNOT TELL IT FROM THE FORM ABOVE. Both are
# anchored, so both pass. This one is nonetheless WRONG in practice -- it misses
# the indented detail shapes. Pinned as a KNOWN LIMIT so nobody reads a green
# checker as proof the predicate is correct.
ANCHORED_BUT_UNSTRIPPED = '''
_REFUSAL_SHAPES = ("refused by:", "validate: REFUSED")
def looks_refused(output):
    return any(
        line.startswith(shape)
        for line in output.splitlines()
        for shape in _REFUSAL_SHAPES
    )
'''

UNRELATED = '''
GREETINGS = ("hello", "hi")
def greets(text):
    return any(g in text for g in GREETINGS)
'''

NAME_TEST = '''
REFUSAL_NAME_HINTS = ("REFUS", "VERDICT")
def is_marker_name(name):
    upper = name.upper()
    return any(hint in upper for hint in REFUSAL_NAME_HINTS)
'''


def self_test() -> int:
    import tempfile

    failures = 0

    def check(name: str, source: str, want: int) -> None:
        nonlocal failures
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "sample.py"
            p.write_text(source, encoding="utf-8")
            got = len(offences(p))
        ok = (got > 0) == (want > 0)
        print(f"  {'ok  ' if ok else 'FAIL'} {name}: offences={got} (wanted {'>0' if want else '0'})")
        if not ok:
            failures += 1

    print("check-refusal-predicate-anchored --self-test")
    print("REFUSES the defect:")
    check("unanchored `shape in output`", UNANCHORED, 1)
    print("PASSES the corrected form -- without this the checker could refuse everything:")
    check("anchored per-line startswith", ANCHORED, 0)
    print("CONTROL -- a non-refusal marker collection is not this checker's business:")
    check("unrelated GREETINGS collection", UNRELATED, 0)
    print("CONTROL -- a NAME test is not a channel classification (this checker's own shape):")
    check("`hint in upper` name comparison", NAME_TEST, 0)
    print("KNOWN LIMIT -- anchored-but-unstripped also passes; only the text above warns:")
    check("bare startswith, misses indented shapes", ANCHORED_BUT_UNSTRIPPED, 0)
    if failures:
        print(f"check-refusal-predicate-anchored --self-test: {failures} case(s) FAILED", file=sys.stderr)
        return 1
    print("check-refusal-predicate-anchored --self-test: all cases pass")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
