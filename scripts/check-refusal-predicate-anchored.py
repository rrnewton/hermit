#!/usr/bin/env python3
"""Refuse a refusal predicate that does not strip and anchor each line.

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

⚠️ STRIP BEFORE ANCHORING. A bare `line.startswith(shape)` is not an
acceptable substitute. Measured by
`agent(hermit-101)` on hermit#2699: `RunSummary::refused` puts its reasons in
`detail` and the renderer indents every detail line by three spaces
(`scripts/validate.rs:11068`), so of the three real shapes only
"validate: REFUSED" is at column zero. A bare `startswith` would have stopped
recognising TWO OF THREE genuine declines -- converting a false positive into a
false negative, which is the worse direction. The checker therefore refuses the
bare form as well as matching against the whole captured log.

⚠️ THIS IS PYTHON, NOT rust-script, AND THAT IS DELIBERATE. `AGENTS.md` prefers
rust-script for new scripts. rust-script compiles, and this was written under an
explicit no-box-time instruction on a busy machine. Porting it to rust-script is
a reasonable follow-up and needs no behaviour change.

EXIT STATUS
    0  no refusal predicate missing the required line handling was found
    1  at least one found -- each named with file, line, and the offending call
    2  usage error / could not parse a file it was asked to check
"""

from __future__ import annotations

import ast
import sys
from pathlib import Path

REFUSAL_TEXT = (
    "refused by:",
    "validate: refused",
    "validate refused",
    "changes-requested",
    "another validate is already running",
)
# Case-folding and trimming a channel yields a channel. Without this,
# `shape in output.lower()` walked straight past -- codex finding 2.
CHANNEL_PRESERVING = ("lower", "upper", "casefold", "strip", "lstrip", "rstrip", "decode")
# A regex search over a whole channel is the same defect wearing a different call.
REGEX_SEARCHES = ("search", "match", "fullmatch", "findall", "finditer")
SUBSTRING_SEARCHES = ("find", "rfind", "index", "rindex", "count")


def _string_literal(node: ast.AST) -> str | None:
    return node.value if isinstance(node, ast.Constant) and isinstance(node.value, str) else None


def _contains_refusal_text(node: ast.AST) -> bool:
    """Whether an expression contains one of the refusal strings we emit."""
    literal = _string_literal(node)
    if literal is not None:
        folded = literal.casefold()
        return any(text in folded for text in REFUSAL_TEXT)
    if isinstance(node, (ast.Tuple, ast.List, ast.Set)):
        return any(_contains_refusal_text(item) for item in node.elts)
    if isinstance(node, ast.Call):
        return any(_contains_refusal_text(arg) for arg in node.args)
    if isinstance(node, ast.JoinedStr):
        return any(
            _contains_refusal_text(value)
            for value in node.values
            if isinstance(value, ast.Constant)
        )
    return False


def _assigned_names(node: ast.Assign | ast.AnnAssign) -> list[str]:
    targets = node.targets if isinstance(node, ast.Assign) else [node.target]
    return [target.id for target in targets if isinstance(target, ast.Name)]


def _marker_collections(tree: ast.Module) -> set[str]:
    """Names whose values contain refusal text, independent of their spelling.

    This includes literal collections and compiled regular expressions. It also
    follows aliases so renaming `_REFUSAL_SHAPES` cannot switch the check off.
    """
    found: set[str] = set()
    assignments = [
        node for node in ast.walk(tree)
        if isinstance(node, (ast.Assign, ast.AnnAssign))
        and node.value is not None
    ]
    changed = True
    while changed:
        changed = False
        for node in assignments:
            value = node.value
            derived = _contains_refusal_text(value) or any(
                isinstance(part, ast.Name) and part.id in found
                for part in ast.walk(value)
            )
            if not derived:
                continue
            for name in _assigned_names(node):
                if name not in found:
                    found.add(name)
                    changed = True
    return found


def _regex_bindings(tree: ast.Module) -> dict[str, tuple[str, bool]]:
    """Compiled refusal regexes as ``name: (pattern, multiline)``."""
    found: dict[str, tuple[str, bool]] = {}
    for node in ast.walk(tree):
        if not isinstance(node, (ast.Assign, ast.AnnAssign)) or node.value is None:
            continue
        value = node.value
        if not (
            isinstance(value, ast.Call)
            and isinstance(value.func, ast.Attribute)
            and isinstance(value.func.value, ast.Name)
            and value.func.value.id == "re"
            and value.func.attr == "compile"
            and value.args
        ):
            continue
        pattern = _string_literal(value.args[0])
        if pattern is None or not _contains_refusal_text(value.args[0]):
            continue
        flags = list(value.args[1:]) + [
            keyword.value for keyword in value.keywords if keyword.arg == "flags"
        ]
        multiline = any(_has_multiline_flag(flag) for flag in flags) or pattern.startswith("(?m)")
        for name in _assigned_names(node):
            found[name] = (pattern, multiline)
    return found


def _has_multiline_flag(node: ast.AST) -> bool:
    return any(
        isinstance(part, ast.Attribute)
        and isinstance(part.value, ast.Name)
        and part.value.id == "re"
        and part.attr in ("M", "MULTILINE")
        for part in ast.walk(node)
    )


def _line_anchored_pattern(pattern: str) -> bool:
    """Whether a regex explicitly accepts indentation only at line start."""
    if pattern.startswith("(?m)"):
        pattern = pattern[4:]
    return pattern.startswith(r"^\s*")


def _containment_helpers(tree: ast.Module) -> dict[str, set[tuple[int, int]]]:
    """Functions whose parameters are compared as ``needle in haystack``."""
    found: dict[str, set[tuple[int, int]]] = {}
    for function in (
        node for node in ast.walk(tree)
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
    ):
        parameters = [arg.arg for arg in function.args.args]
        positions = {name: index for index, name in enumerate(parameters)}
        for node in ast.walk(function):
            if not (
                isinstance(node, ast.Compare)
                and len(node.ops) == 1
                and isinstance(node.ops[0], ast.In)
                and isinstance(node.left, ast.Name)
                and isinstance(node.comparators[0], ast.Name)
            ):
                continue
            needle = positions.get(node.left.id)
            haystack = positions.get(node.comparators[0].id)
            if needle is not None and haystack is not None:
                found.setdefault(function.name, set()).add((needle, haystack))
    return found


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

    def __init__(
        self,
        markers: set[str],
        regexes: dict[str, tuple[str, bool]],
        helpers: dict[str, set[tuple[int, int]]],
    ) -> None:
        self.markers = markers
        self.regexes = regexes
        self.helpers = helpers
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
            return _contains_refusal_text(node)
        if isinstance(node, ast.Subscript):
            return self._marker_derived(node.value)
        if isinstance(node, ast.Call):
            # `re.escape(shape)` is still the marker -- claude-lane rewrite 1.
            return any(self._marker_derived(a) for a in node.args)
        if isinstance(node, ast.JoinedStr):
            return any(
                self._marker_derived(v.value)
                for v in node.values
                if isinstance(v, ast.FormattedValue)
            )
        return False

    def _pattern(self, node: ast.AST) -> tuple[str, bool] | None:
        literal = _string_literal(node)
        if literal is not None:
            return literal, literal.startswith("(?m)")
        if isinstance(node, ast.Name):
            return self.regexes.get(node.id)
        return None

    def _regex_is_anchored_per_line(
        self,
        pattern_node: ast.AST,
        string_node: ast.AST,
        flags: list[ast.AST],
    ) -> bool:
        pattern = self._pattern(pattern_node)
        if pattern is None or not _line_anchored_pattern(pattern[0]):
            return False
        if self._line_bound(string_node):
            return True
        return pattern[1] or any(_has_multiline_flag(flag) for flag in flags)

    def _leading_whitespace_removed(self, node: ast.AST) -> bool:
        """Whether a line-bound value has had leading whitespace removed."""
        if not isinstance(node, ast.Call) or not isinstance(node.func, ast.Attribute):
            return False
        method = node.func.attr
        if method in ("strip", "lstrip"):
            return not node.args and not node.keywords and self._line_bound(node.func.value)
        if method in CHANNEL_PRESERVING:
            return self._leading_whitespace_removed(node.func.value)
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
            # ⚠️ NO LONGER REQUIRES THE HAYSTACK TO BE NAMED LIKE A CHANNEL.
            # `any(s in buf for s in REFUSAL_SHAPES)` -- a pure rename -- used to
            # pass. Direction still matters (codex finding 3): the safe
            # `output in _REFUSAL_SHAPES` has a marker-derived HAYSTACK.
            if (
                self._marker_derived(needle)
                and not self._marker_derived(haystack)
            ):
                self.hits.append((node.lineno, ast.unparse(node)))
        self.generic_visit(node)

    def visit_Call(self, node: ast.Call) -> None:
        f = node.func
        # `line.startswith(shape)` is anchored at column zero, but real refusal
        # detail lines are indented by the validate renderer. Require the line to
        # have leading whitespace removed before startswith, or two of the three
        # genuine decline forms are missed. Also refuse start/end arguments: a
        # nonzero start means the check is no longer anchored at the beginning.
        if (
            isinstance(f, ast.Attribute)
            and f.attr == "startswith"
            and len(node.args) >= 1
            and self._marker_derived(node.args[0])
            and (
                not self._line_bound(f.value)
                or
                len(node.args) != 1
                or node.keywords
                or not self._leading_whitespace_removed(f.value)
            )
        ):
            self.hits.append((node.lineno, ast.unparse(node)))
        elif (
            isinstance(f, ast.Attribute)
            and f.attr == "endswith"
            and node.args
            and self._marker_derived(node.args[0])
        ):
            self.hits.append((node.lineno, ast.unparse(node)))
        # `output.find(shape) >= 0` -- same test, different method. claude rewrite 2.
        elif (
            isinstance(f, ast.Attribute)
            and f.attr in SUBSTRING_SEARCHES
            and len(node.args) >= 1
            and self._marker_derived(node.args[0])
            and not self._marker_derived(f.value)
        ):
            self.hits.append((node.lineno, ast.unparse(node)))
        # `_contains(output, shape)` -- one level of indirection. Follow the
        # helper's parameter roles instead of treating every multi-argument call
        # as containment; ordinary logging of a marker is harmless.
        elif isinstance(f, ast.Name) and f.id in self.helpers:
            for needle_index, haystack_index in self.helpers[f.id]:
                if (
                    needle_index < len(node.args)
                    and haystack_index < len(node.args)
                    and self._marker_derived(node.args[needle_index])
                    and not self._marker_derived(node.args[haystack_index])
                ):
                    self.hits.append((node.lineno, ast.unparse(node)))
                    break
        if (
            isinstance(f, ast.Attribute)
            and f.attr in REGEX_SEARCHES
            and isinstance(f.value, ast.Name)
            and f.value.id == "re"
            and len(node.args) >= 2
        ):
            pat, string = node.args[0], node.args[1]
            flags = list(node.args[2:]) + [
                keyword.value for keyword in node.keywords if keyword.arg == "flags"
            ]
            safe = f.attr == "fullmatch" and self._line_bound(string)
            if f.attr == "match":
                safe = self._line_bound(string) and self._leading_whitespace_removed(string)
            elif f.attr in ("search", "findall", "finditer"):
                safe = self._regex_is_anchored_per_line(pat, string, flags)
            if self._marker_derived(pat) and not safe:
                self.hits.append((node.lineno, ast.unparse(node)))
        elif (
            isinstance(f, ast.Attribute)
            and f.attr in REGEX_SEARCHES
            and self._marker_derived(f.value)
            and node.args
        ):
            string = node.args[0]
            pattern = self._pattern(f.value)
            safe = f.attr == "fullmatch" and self._line_bound(string)
            if f.attr == "match":
                safe = self._line_bound(string) and self._leading_whitespace_removed(string)
            elif f.attr in ("search", "findall", "finditer") and pattern is not None:
                safe = self._regex_is_anchored_per_line(f.value, string, [])
            if not safe:
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
    scan = _Scan(markers, _regex_bindings(tree), _containment_helpers(tree))
    scan.visit(tree)
    return sorted(set(scan.hits))


REMEDY = (
    "  Match each line, not the whole captured channel. Use fullmatch for a\n"
    "  complete status-line regex, or strip before a prefix comparison:\n"
    "      any(line.strip().startswith(m) for line in output.splitlines() for m in MARKERS)\n"
    "  Indented detail lines are real declines, so a bare startswith misses them."
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
    this_file = Path(__file__).resolve()
    candidates = root.rglob("*.py") if root.is_dir() else [root]
    targets = sorted(
        path for path in candidates
        if ".git" not in path.parts and path.resolve() != this_file
    )
    # ⚠️ FAIL CLOSED ON AN EMPTY CORPUS. This previously printed
    # "OK -- no unanchored refusal predicate" and returned 0 when it had scanned
    # NOTHING. A rename, a move, or a typo in the Makefile line would have
    # silently disabled the gate while it reported success -- a check that passes
    # because it has nothing to check, which is the defect class this whole file
    # exists to remove, reproduced inside the guard built to prevent it.
    # `a-corpus-test-must-fail-closed-on-empty` is the standing rule here.
    if not targets:
        print(
            f"check-refusal-predicate-anchored: REFUSED -- scanned ZERO files under "
            f"{root}.\n"
            "  An empty corpus is not a pass. Either the path is wrong (a rename, a\n"
            "  move, or a typo in the invoking recipe) or the tree really has no\n"
            "  Python, and neither is evidence that no unanchored predicate exists.",
            file=sys.stderr,
        )
        return 1
    bad = 0
    for path in targets:
        try:
            found = offences(path)
        except Unreadable as error:
            print(f"check-refusal-predicate-anchored: {error}", file=sys.stderr)
            return 2
        for lineno, src in found:
            bad += 1
            print(
                f"{path}:{lineno}: refusal marker is not matched with "
                f"line.strip().startswith(...): {src}",
                file=sys.stderr,
            )
    if bad:
        print(
            f"\ncheck-refusal-predicate-anchored: REFUSED -- {bad} refusal "
            f"predicate(s) do not strip and anchor each line.\n" + REMEDY,
            file=sys.stderr,
        )
        return 1
    print(
        "check-refusal-predicate-anchored: OK -- every refusal predicate strips "
        "and anchors each line."
    )
    return 0


# ── fixtures ──────────────────────────────────────────────────────────────────
# Every REFUSE case below is one the codex lane demonstrated passing at
# c7325dff3073. They are pinned here so the same miss cannot return silently.

F_UNANCHORED = '''
_REFUSAL_SHAPES = ("refused by:", "validate: REFUSED")
def looks_refused(output):
    return any(shape in output for shape in _REFUSAL_SHAPES)
'''

F_LINE_CONTAINMENT = '''
_REFUSAL_SHAPES = ("refused by:",)
def looks_refused(output):
    return any(
        shape in line
        for line in output.splitlines()
        for shape in _REFUSAL_SHAPES
    )
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

F_REGEX_RENAMED_CHANNEL = '''
import re
_REFUSAL_SHAPES = ("refused by:",)
def looks_refused(buf):
    return any(re.search(shape, buf) for shape in _REFUSAL_SHAPES)
'''

F_REGEX_ANCHORED = r'''
import re
def looks_refused(output):
    return re.search(r"^\s*(?:refused by:|validate: REFUSED)", output, re.MULTILINE)
'''

F_COMPILED_SEARCH = r'''
import re
_REFUSAL_SUMMARY = re.compile(r"validate REFUSED \(exit [1-9][0-9]*\)")
def looks_refused(output):
    return bool(_REFUSAL_SUMMARY.search(output))
'''

F_COMPILED_FULLMATCH = r'''
import re
_REFUSAL_SUMMARY = re.compile(r"validate REFUSED \(exit [1-9][0-9]*\)")
def looks_refused(output):
    return any(_REFUSAL_SUMMARY.fullmatch(line) for line in output.splitlines())
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

F_STARTS_AFTER_BEGINNING = '''
_REFUSAL_SHAPES = ("refused by:",)
def looks_refused(output):
    return any(
        line.strip().startswith(shape, 1)
        for line in output.splitlines()
        for shape in _REFUSAL_SHAPES
    )
'''

F_EXACT_MEMBERSHIP = '''
_REFUSAL_SHAPES = ("refused by:",)
def is_exact(output):
    return output in _REFUSAL_SHAPES
'''

F_RENAMED_COLLECTION = '''
OUTCOMES = ("refused by:",)
def looks_refused(output):
    return any(item in output for item in OUTCOMES)
'''

F_DIRECT_LITERAL = '''
def looks_refused(output):
    return "refused by:" in output
'''

F_ENDSWITH = '''
_REFUSAL_SHAPES = ("refused by:",)
def looks_refused(output):
    return any(output.endswith(shape) for shape in _REFUSAL_SHAPES)
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


F_REGEX_ESCAPE = '''
import re
_REFUSAL_SHAPES = ("refused by:",)
def looks_refused(output):
    return any(re.search(re.escape(s), output) for s in _REFUSAL_SHAPES)
'''

F_FIND = '''
_REFUSAL_SHAPES = ("refused by:",)
def looks_refused(output):
    return any(output.find(s) >= 0 for s in _REFUSAL_SHAPES)
'''

F_RENAMED_HAYSTACK = '''
_REFUSAL_SHAPES = ("refused by:",)
def looks_refused(buf):
    return any(s in buf for s in _REFUSAL_SHAPES)
'''

F_INDIRECTION = '''
_REFUSAL_SHAPES = ("refused by:",)
def _contains(hay, needle):
    return needle in hay
def looks_refused(output):
    return any(_contains(output, s) for s in _REFUSAL_SHAPES)
'''

F_BENIGN_CALL = '''
_REFUSAL_SHAPES = ("refused by:",)
def report():
    for shape in _REFUSAL_SHAPES:
        print(shape)
        collected.append(shape)
'''

F_BENIGN_LOGGING = '''
import logging
_REFUSAL_SHAPES = ("refused by:",)
def report():
    for shape in _REFUSAL_SHAPES:
        logging.info("checking %s", shape)
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
    check("line splitting without a start comparison", F_LINE_CONTAINMENT, True)
    check("codex-1 unrelated splitlines elsewhere in the file", F_SCOPE_LEAK, True)
    check("codex-2 annotated marker collection", F_ANNOTATED, True)
    check("codex-2 statement `for` loop, not a comprehension", F_FOR_LOOP, True)
    check("codex-2 `shape in output.lower()`", F_LOWER, True)
    check("codex-2 unanchored `re.search(shape, output)`", F_REGEX, True)
    check("renaming the regex channel", F_REGEX_RENAMED_CHANNEL, True)
    check("compiled regex search over the whole channel", F_COMPILED_SEARCH, True)
    check("renaming the marker collection", F_RENAMED_COLLECTION, True)
    check("direct refusal literal membership", F_DIRECT_LITERAL, True)
    check("endswith does not establish line-start anchoring", F_ENDSWITH, True)
    print("REFUSES the four one-line rewrites the claude lane demonstrated evading it:")
    check("claude-1 `re.search(re.escape(s), output)`", F_REGEX_ESCAPE, True)
    check("claude-2 `output.find(s) >= 0`", F_FIND, True)
    check("claude-3 haystack merely RENAMED to `buf`", F_RENAMED_HAYSTACK, True)
    check("claude-4 indirection via `_contains(output, s)`", F_INDIRECTION, True)
    print("PASSES what is safe -- without these the gate could refuse everything:")
    check("anchored strip().startswith", F_ANCHORED, False)
    check("anchored multiline regex", F_REGEX_ANCHORED, False)
    check("compiled fullmatch over split lines", F_COMPILED_FULLMATCH, False)
    check("codex-3 `output in _REFUSAL_SHAPES` exact membership", F_EXACT_MEMBERSHIP, False)
    check("unrelated marker collection", F_UNRELATED, False)
    check("name test, this checker's own shape", F_NAME_TEST, False)
    check("benign single-arg uses of a marker are NOT flagged", F_BENIGN_CALL, False)
    check("logging a marker is not a refusal predicate", F_BENIGN_LOGGING, False)
    print("REFUSES startswith without removing indentation first:")
    check("bare startswith, misses indented shapes", F_UNSTRIPPED, True)
    check("startswith begins after the start of the line", F_STARTS_AFTER_BEGINNING, True)
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
        # ⚠️ THE SERIOUS ONE. A check that passes with nothing to check.
        empty = Path(d) / "empty_corpus"
        empty.mkdir()
        check_rc("EMPTY CORPUS FAILS CLOSED, not 0", 1, str(empty))

    if failures:
        print(f"check-refusal-predicate-anchored --self-test: {failures} case(s) FAILED", file=sys.stderr)
        return 1
    print("check-refusal-predicate-anchored --self-test: all cases pass")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
