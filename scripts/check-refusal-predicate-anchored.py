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

or a regex anchored per line under `re.MULTILINE`. Anchoring is what distinguishes the
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

MARKER_NAME_HINTS = ("REFUS", "VERDICT", "DECLINE")
CHANNEL_NAME_HINTS = (
    "output", "log", "stdout", "stderr", "captured", "seen", "text",
    "content", "body", "blob", "sample",
)
# Case-folding and trimming a channel yields a channel. Without this,
# `shape in output.lower()` walked straight past -- codex finding 2.
CHANNEL_PRESERVING = ("lower", "upper", "casefold", "strip", "lstrip", "rstrip", "decode")
# A regex search over a whole channel is the same defect wearing a different call.
REGEX_SEARCHES = ("search", "match", "fullmatch", "findall", "finditer")


def _is_marker_name(name: str) -> bool:
    return any(h in name.upper() for h in MARKER_NAME_HINTS)


def _marker_collections(tree: ast.Module) -> set[str]:
    """Module-level names bound to a tuple/list/set of string literals.

    Handles `X = (...)` and `X: tuple[str, ...] = (...)`; the annotated form was
    invisible before -- codex finding 2.
    """
    found: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Assign):
            targets, value = node.targets, node.value
        elif isinstance(node, ast.AnnAssign) and node.value is not None:
            targets, value = [node.target], node.value
        else:
            continue
        if not isinstance(value, (ast.Tuple, ast.List, ast.Set)):
            continue
        elts = value.elts
        if not elts or not all(
            isinstance(e, ast.Constant) and isinstance(e.value, str) for e in elts
        ):
            continue
        for t in targets:
            if isinstance(t, ast.Name) and _is_marker_name(t.id):
                found.add(t.id)
    return found


def _is_channel(node: ast.AST) -> bool:
    if isinstance(node, ast.Name):
        return any(h in node.id.lower() for h in CHANNEL_NAME_HINTS)
    if isinstance(node, ast.Attribute):
        return any(h in node.attr.lower() for h in CHANNEL_NAME_HINTS)
    if isinstance(node, ast.Call):
        f = node.func
        if isinstance(f, ast.Attribute) and f.attr in CHANNEL_PRESERVING:
            return _is_channel(f.value)
        return _is_channel(f)
    return False


def _splitlines_target(gen_or_for) -> str | None:
    """The name this iteration binds to ONE LINE, if it iterates `.splitlines()`."""
    it = gen_or_for.iter
    if (
        isinstance(it, ast.Call)
        and isinstance(it.func, ast.Attribute)
        and it.func.attr == "splitlines"
        and isinstance(gen_or_for.target, ast.Name)
    ):
        return gen_or_for.target.id
    return None


def _marker_target(gen_or_for, markers: set[str]) -> str | None:
    """The name this iteration binds to ONE MARKER, if it iterates a marker set."""
    it = gen_or_for.iter
    if isinstance(it, ast.Name) and it.id in markers and isinstance(gen_or_for.target, ast.Name):
        return gen_or_for.target.id
    return None


class _Scan(ast.NodeVisitor):
    """⚠️ SCOPED, NOT MODULE-WIDE.

    The previous version collected every `for x in y.splitlines()` target in the
    file into one set, so an unrelated comprehension anywhere made the exact
    forbidden expression pass -- codex finding 1, reproduced. Line-bound and
    marker-bound names are now pushed and popped with the comprehension or `for`
    that binds them, so a name is only exempt INSIDE the construct that anchors it.
    """

    def __init__(self, markers: set[str]) -> None:
        self.markers = markers
        self.lines: list[set[str]] = [set()]
        self.marks: list[set[str]] = [set()]
        self.hits: list[tuple[int, str]] = []

    def _line_bound(self, node: ast.AST) -> bool:
        if isinstance(node, ast.Name):
            return any(node.id in frame for frame in self.lines)
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            if node.func.attr in CHANNEL_PRESERVING:
                return self._line_bound(node.func.value)
        return False

    def _marker_derived(self, node: ast.AST) -> bool:
        if isinstance(node, ast.Name):
            return node.id in self.markers or any(node.id in f for f in self.marks)
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            return False
        if isinstance(node, ast.Subscript):
            return self._marker_derived(node.value)
        return False

    def _push(self, gens) -> int:
        n = 0
        for g in gens:
            lt, mt = _splitlines_target(g), _marker_target(g, self.markers)
            self.lines.append({lt} if lt else set())
            self.marks.append({mt} if mt else set())
            n += 1
        return n

    def _pop(self, n: int) -> None:
        for _ in range(n):
            self.lines.pop()
            self.marks.pop()

    def _visit_comp(self, node) -> None:
        n = self._push(node.generators)
        self.generic_visit(node)
        self._pop(n)

    visit_GeneratorExp = _visit_comp
    visit_ListComp = _visit_comp
    visit_SetComp = _visit_comp

    def visit_For(self, node: ast.For) -> None:
        # `for shape in _REFUSAL_SHAPES:` as a statement, not a comprehension --
        # codex finding 2.
        n = self._push([node])
        self.generic_visit(node)
        self._pop(n)

    def visit_Compare(self, node: ast.Compare) -> None:
        if len(node.ops) == 1 and isinstance(node.ops[0], ast.In):
            needle, haystack = node.left, node.comparators[0]
            # ⚠️ DIRECTION MATTERS -- codex finding 3. `output in _REFUSAL_SHAPES`
            # is an EXACT-VALUE membership and is safe; only marker-as-needle
            # against channel-as-haystack is the defect.
            if (
                self._marker_derived(needle)
                and not self._marker_derived(haystack)
                and _is_channel(haystack)
                and not self._line_bound(haystack)
            ):
                self.hits.append((node.lineno, ast.unparse(node)))
        self.generic_visit(node)

    def visit_Call(self, node: ast.Call) -> None:
        f = node.func
        if (
            isinstance(f, ast.Attribute)
            and f.attr in REGEX_SEARCHES
            and isinstance(f.value, ast.Name)
            and f.value.id == "re"
            and len(node.args) >= 2
        ):
            pat, string = node.args[0], node.args[1]
            if (
                self._marker_derived(pat)
                and _is_channel(string)
                and not self._line_bound(string)
            ):
                self.hits.append((node.lineno, ast.unparse(node)))
        self.generic_visit(node)


class Unreadable(Exception):
    pass


def offences(path: Path) -> list[tuple[int, str]]:
    try:
        source = path.read_text(encoding="utf-8")
    except OSError as error:
        raise Unreadable(f"cannot read {path}: {error}") from error
    try:
        tree = ast.parse(source, filename=str(path))
    except SyntaxError as error:
        raise Unreadable(f"cannot parse {path}: {error}") from error
    markers = _marker_collections(tree)
    if not markers:
        return []
    scan = _Scan(markers)
    scan.visit(tree)
    return sorted(set(scan.hits))


REMEDY = (
    "  Match the marker anchored to the start of a line, stripping first:\n"
    "      any(line.strip().startswith(m) for line in output.splitlines() for m in MARKERS)\n"
    "  STRIP FIRST: indented detail lines are real declines and a bare startswith\n"
    "  misses them, turning a false positive into a false negative."
)


def main(argv: list[str]) -> int:
    args = argv[1:]
    if args and args[0] == "--self-test":
        if len(args) != 1:
            print("usage: check-refusal-predicate-anchored.py [--self-test | PATH]", file=sys.stderr)
            return 2
        return self_test()
    # ⚠️ EXIT-STATUS CONTRACT, codex finding 4: usage / unreadable / unparseable
    # are 2, not 1 and not a silent 0. An extra positional used to be ignored.
    if len(args) > 1:
        print(
            f"usage: check-refusal-predicate-anchored.py [--self-test | PATH]\n"
            f"  got {len(args)} positional arguments: {args}",
            file=sys.stderr,
        )
        return 2
    root = Path(args[0]) if args else Path(".")
    if not root.exists():
        print(f"check-refusal-predicate-anchored: no such path: {root}", file=sys.stderr)
        return 2
    targets = sorted(root.rglob("*.py")) if root.is_dir() else [root]
    bad = 0
    for path in targets:
        if "/.git/" in str(path):
            continue
        try:
            found = offences(path)
        except Unreadable as error:
            print(f"check-refusal-predicate-anchored: {error}", file=sys.stderr)
            return 2
        for lineno, src in found:
            bad += 1
            print(
                f"{path}:{lineno}: refusal marker matched UNANCHORED against a whole "
                f"channel: {src}",
                file=sys.stderr,
            )
    if bad:
        print(
            f"\ncheck-refusal-predicate-anchored: REFUSED -- {bad} unanchored "
            f"refusal predicate(s).\n" + REMEDY,
            file=sys.stderr,
        )
        return 1
    print("check-refusal-predicate-anchored: OK -- no unanchored refusal predicate.")
    return 0


# ── fixtures ──────────────────────────────────────────────────────────────────
# Every REFUSE case below is one the codex lane demonstrated passing at
# c7325dff3073. They are pinned here so the same miss cannot return silently.

F_UNANCHORED = '''
_REFUSAL_SHAPES = ("refused by:", "validate: REFUSED")
def looks_refused(output):
    return any(shape in output for shape in _REFUSAL_SHAPES)
'''

F_SCOPE_LEAK = '''
_REFUSAL_SHAPES = ("refused by:",)
def unrelated(text):
    return [output for output in text.splitlines()]
def looks_refused(output):
    return any(shape in output for shape in _REFUSAL_SHAPES)
'''

F_ANNOTATED = '''
_REFUSAL_SHAPES: tuple[str, ...] = ("refused by:",)
def looks_refused(output):
    return any(shape in output for shape in _REFUSAL_SHAPES)
'''

F_FOR_LOOP = '''
_REFUSAL_SHAPES = ("refused by:",)
def looks_refused(output):
    for shape in _REFUSAL_SHAPES:
        if shape in output:
            return True
    return False
'''

F_LOWER = '''
_REFUSAL_SHAPES = ("refused by:",)
def looks_refused(output):
    return any(shape in output.lower() for shape in _REFUSAL_SHAPES)
'''

F_REGEX = '''
import re
_REFUSAL_SHAPES = ("refused by:",)
def looks_refused(output):
    return any(re.search(shape, output) for shape in _REFUSAL_SHAPES)
'''

F_ANCHORED = '''
_REFUSAL_SHAPES = ("refused by:",)
def looks_refused(output):
    return any(
        line.strip().startswith(shape)
        for line in output.splitlines()
        for shape in _REFUSAL_SHAPES
    )
'''

F_UNSTRIPPED = '''
_REFUSAL_SHAPES = ("refused by:",)
def looks_refused(output):
    return any(
        line.startswith(shape) for line in output.splitlines() for shape in _REFUSAL_SHAPES
    )
'''

F_EXACT_MEMBERSHIP = '''
_REFUSAL_SHAPES = ("refused by:",)
def is_exact(output):
    return output in _REFUSAL_SHAPES
'''

F_UNRELATED = '''
GREETINGS = ("hello", "hi")
def greets(text):
    return any(g in text for g in GREETINGS)
'''

F_NAME_TEST = '''
REFUSAL_NAME_HINTS = ("REFUS",)
def is_marker_name(name):
    upper = name.upper()
    return any(hint in upper for hint in REFUSAL_NAME_HINTS)
'''


def self_test() -> int:
    import subprocess
    import tempfile

    failures = 0
    me = Path(__file__).resolve()

    def check(name: str, source: str, want_hit: bool) -> None:
        nonlocal failures
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "sample.py"
            f.write_text(source, encoding="utf-8")
            got = len(offences(f))
        ok = (got > 0) == want_hit
        print(f"  {'ok  ' if ok else 'FAIL'} {name}: offences={got}")
        failures += 0 if ok else 1

    def rc_of(*a: str) -> int:
        return subprocess.run([sys.executable, str(me), *a], capture_output=True).returncode

    def check_rc(name: str, want: int, *a: str) -> None:
        nonlocal failures
        got = rc_of(*a)
        ok = got == want
        print(f"  {'ok  ' if ok else 'FAIL'} {name}: rc={got} (wanted {want})")
        failures += 0 if ok else 1

    print("check-refusal-predicate-anchored --self-test")
    print("REFUSES the defect, in every form the codex lane demonstrated slipping through:")
    check("plain `shape in output`", F_UNANCHORED, True)
    check("codex-1 unrelated splitlines elsewhere in the file", F_SCOPE_LEAK, True)
    check("codex-2 annotated marker collection", F_ANNOTATED, True)
    check("codex-2 statement `for` loop, not a comprehension", F_FOR_LOOP, True)
    check("codex-2 `shape in output.lower()`", F_LOWER, True)
    check("codex-2 unanchored `re.search(shape, output)`", F_REGEX, True)
    print("PASSES what is safe -- without these the gate could refuse everything:")
    check("anchored strip().startswith", F_ANCHORED, False)
    check("codex-3 `output in _REFUSAL_SHAPES` exact membership", F_EXACT_MEMBERSHIP, False)
    check("unrelated marker collection", F_UNRELATED, False)
    check("name test, this checker's own shape", F_NAME_TEST, False)
    print("KNOWN LIMIT -- anchored-but-unstripped passes; only the remedy text warns:")
    check("bare startswith, misses indented shapes", F_UNSTRIPPED, False)
    print("codex-4 EXIT-STATUS CONTRACT:")
    with tempfile.TemporaryDirectory() as d:
        broken = Path(d) / "broken.py"
        broken.write_text("def f(:\n", encoding="utf-8")
        check_rc("unparseable file is 2, not 1", 2, str(broken))
        check_rc("missing path is 2", 2, str(Path(d) / "absent.py"))
        clean = Path(d) / "ok.py"
        clean.write_text("x = 1\n", encoding="utf-8")
        check_rc("clean file is 0", 0, str(clean))
        check_rc("extra positional is 2, not silently 0", 2, str(clean), str(clean))

    if failures:
        print(f"check-refusal-predicate-anchored --self-test: {failures} case(s) FAILED", file=sys.stderr)
        return 1
    print("check-refusal-predicate-anchored --self-test: all cases pass")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
