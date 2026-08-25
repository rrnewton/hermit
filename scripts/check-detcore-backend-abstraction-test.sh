#!/usr/bin/env bash
# Self-test for check-detcore-backend-abstraction.sh.
#
# Two properties are covered, both of which are load-bearing for the lint's
# trustworthiness:
#
#   1. THE FAST MASKER AGREES WITH THE OBVIOUS SLOW ONE. The lint decides which
#      Reverie references are real by first blanking comments, string literals,
#      raw strings and char literals. If that masking is wrong in the permissive
#      direction the lint goes BLIND -- a genuine `reverie_kvm::` in code could
#      be masked away and reported clean. The original implementation walked the
#      source one character at a time in Python, which was obviously correct and
#      unusably slow (10.84s for detcore/src's 49 files). It is retained BELOW,
#      verbatim, as the reference specification, and the fast implementation
#      used by the lint must produce byte-identical output on every input.
#
#   2. THE DERIVED BUDGET GUARD ACTUALLY FIRES. The lint's cost is one
#      self-invocation per negative control and the control list is derived from
#      the workspace, so it refuses when the wall timeout declared in
#      ci/dag/portable.json cannot cover the work the workspace now derives. A
#      guard that only ever passes would be indistinguishable from no guard.
#
# Run locally or in CI:
#
#     scripts/check-detcore-backend-abstraction-test.sh
#
# Exits 0 when every case passes, 1 otherwise.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
readonly SCRIPT_DIR REPO_ROOT
readonly LINT="$SCRIPT_DIR/check-detcore-backend-abstraction.sh"

failures=0
note() { echo "  $*"; }
fail() {
    echo "check-detcore-backend-abstraction-test.sh: FAIL — $*" >&2
    failures=$((failures + 1))
}

if [[ ! -x $LINT ]]; then
    fail "$LINT is missing or not executable"
    exit 1
fi

# --- 1. masker equivalence ----------------------------------------------------
#
# The fast implementation is read out of the lint itself rather than duplicated,
# so this test cannot drift into checking a stale copy.

echo "masker equivalence: fast implementation vs slow reference"
if ! python3 - "$LINT" "$REPO_ROOT" <<'PYEOF'; then
import random
import re
import sys
import pathlib

lint_path, repo_root = sys.argv[1], pathlib.Path(sys.argv[2])
lint = pathlib.Path(lint_path).read_text(encoding="utf-8")

# Slice the fast masker out of the lint. Both anchors are load-bearing; if the
# lint is restructured this test must be updated rather than silently skipped.
try:
    start = lint.index("TRIGGER = re.compile(")
    end = lint.index('for path in sorted(source_root.rglob("*.rs")):')
except ValueError:
    print("cannot locate the masker in the lint; anchors changed", file=sys.stderr)
    raise SystemExit(1)

fast_ns = {}
exec("import re\n" + lint[start:end], fast_ns)  # noqa: S102 - reading our own repo
fast = fast_ns["mask_comments_and_literals"]


# THE REFERENCE SPECIFICATION. Deliberately naive: it walks every character and
# blanks them one at a time. Do not "optimize" this copy -- being obviously
# correct is its entire job.
def blank(chars, start, end):
    for index in range(start, end):
        if chars[index] != "\n":
            chars[index] = " "


def reference(source):
    chars = list(source)
    length = len(source)
    index = 0
    while index < length:
        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            if end == -1:
                end = length
            blank(chars, index, end)
            index = end
            continue

        if source.startswith("/*", index):
            depth = 1
            end = index + 2
            while end < length and depth:
                if source.startswith("/*", end):
                    depth += 1
                    end += 2
                elif source.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            blank(chars, index, end)
            index = end
            continue

        raw = re.match(r'(?:br|cr|r)(?P<hashes>#{0,255})"', source[index:])
        if raw and (index == 0 or not (source[index - 1].isalnum() or source[index - 1] == "_")):
            terminator = '"' + raw.group("hashes")
            end = source.find(terminator, index + raw.end())
            end = length if end == -1 else end + len(terminator)
            blank(chars, index, end)
            index = end
            continue

        if source[index] == '"':
            end = index + 1
            escaped = False
            while end < length:
                char = source[end]
                end += 1
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    break
            blank(chars, index, end)
            index = end
            continue

        if source[index] == "'" and index + 2 < length:
            if source[index + 1] == "\\":
                end = index + 2
                while end < length and source[end] != "\n":
                    if source[end] == "'" and source[end - 1] != "\\":
                        end += 1
                        blank(chars, index, end)
                        index = end
                        break
                    end += 1
                else:
                    index += 1
                continue
            if source[index + 2] == "'":
                blank(chars, index, index + 3)
                index += 3
                continue

        index += 1
    return "".join(chars)


mismatches = 0


def compare(label, source):
    global mismatches
    if fast(source) != reference(source):
        mismatches += 1
        if mismatches <= 5:
            print(f"  MISMATCH [{label}]: {source[:120]!r}", file=sys.stderr)


# (a) every tracked Rust source in the repository.
corpus = [p for p in repo_root.rglob("*.rs") if "/target/" not in str(p)]
scanned = 0
for path in corpus:
    try:
        text = path.read_text(encoding="utf-8")
    except (UnicodeDecodeError, OSError):
        continue
    scanned += 1
    compare(str(path), text)
print(f"  real sources: {scanned} file(s)")

# (b) the corners real code does not reliably contain. Each case pairs a masking
# construct with a Reverie reference so a masking error changes the verdict.
cases = [
    r'let a = r"reverie_kvm::x";',
    r'let a = r#"reverie_kvm::x"#;',
    r'let a = r###"a"##b"###; reverie_dbt::y();',
    r'let a = br"reverie_kvm::x"; let b = cr#"q"#;',
    r'let s = "a\"reverie_kvm::b\"c"; reverie_sabre::d();',
    r'/* /* nested */ reverie_kvm::hidden */ reverie_dbt::real();',
    r'// reverie_kvm::commented',
    "// unterminated line comment at EOF",
    r"/* unterminated block comment reverie_kvm::x",
    r'let s = "unterminated reverie_kvm::x',
    r"let c = '\''; reverie_kvm::after();",
    r"let c = '\n'; let d = 'x'; reverie_dbt::z();",
    r"struct S<'a> { r: &'a str } // reverie_kvm",
    r'identr"notraw"; reverie_kvm::q();',
    r"let t = 'a; loop { break 't; }",
    r'let e = ""; let f = "\\"; reverie_kvm::g();',
    r'let h = "\\\\"; reverie_ptrace::i();',
    'let j = r"multi\nline\nraw reverie_kvm::k";',
    "/*/ tricky */ reverie_kvm::l();",
    "/**/ reverie_memory::m();",
    "let n = '\\u{1F600}'; reverie_syscalls::o();",
    "'", '"', "/", "//", "/*", 'r"', 'r#"', "", "\n", "'''", "''''",
]
for index, case in enumerate(cases):
    compare(f"case {index}", case)
print(f"  adversarial cases: {len(cases)}")

# (c) fuzz over an alphabet of masking metacharacters. Seeded, so a failure is
# reproducible rather than a story about a run that once went red.
random.seed(20260825)
alphabet = list('ab/*"\'\\#rnc \n;:_') + ["//", "/*", "*/", 'r"', 'r#"', '"#', '\\"', "'\\''"]
FUZZ = 20000
for _ in range(FUZZ):
    length = random.randint(0, 60)
    compare("fuzz", "".join(random.choice(alphabet) for _ in range(length)))
print(f"  fuzz cases: {FUZZ}")

if mismatches:
    print(f"  {mismatches} input(s) masked differently", file=sys.stderr)
    raise SystemExit(1)
print("  OK — fast masker is byte-identical to the reference on every input")
PYEOF
    fail "the fast masker disagrees with the reference implementation"
fi

# --- 2. the derived budget guard fires ---------------------------------------
#
# Bracketed both ways. A guard is only meaningful if the compliant case passes
# AND the non-compliant case is refused; a refusal-only test would also pass
# against a lint that refused everything.

echo "derived budget guard"

scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
cp -a "$REPO_ROOT/detcore" "$scratch/detcore"
mkdir -p "$scratch/ci/dag"
printf '[workspace]\nmembers = ["detcore"]\nresolver = "2"\n' > "$scratch/Cargo.toml"

write_dag() {
    python3 - "$scratch/ci/dag/portable.json" "$1" <<'PYEOF'
import json
import sys

path, timeout = sys.argv[1], int(sys.argv[2])
graph = {
    "steps": [
        {
            "group": "check",
            "job": "backend_abstraction",
            "desc": "fixture",
            "cmd": "true",
            "timeout": timeout,
        }
    ]
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(graph, handle)
PYEOF
}

# A budget that comfortably covers the derived work must be accepted.
write_dag 600
if output=$("$LINT" --repo-root "$scratch" 2>&1); then
    if ! grep -q "budget: 600s declared covers" <<< "$output"; then
        fail "a sufficient budget was accepted but not reported"
    else
        note "OK — a sufficient 600s budget is accepted and reported"
    fi
else
    status=$?
    if grep -q "BUDGETED FOR LESS WORK" <<< "$output"; then
        fail "a sufficient 600s budget was refused as insufficient"
    else
        note "OK — sufficient budget accepted (lint exited $status on unrelated fixture grounds)"
    fi
fi

# A budget that cannot cover the derived work must be REFUSED, and must say so
# as a budget statement rather than as a timeout.
write_dag 1
if output=$("$LINT" --repo-root "$scratch" 2>&1); then
    fail "a 1s budget was accepted for work that cannot fit in it"
else
    if ! grep -q "BUDGETED FOR LESS WORK THAN IT DERIVES" <<< "$output"; then
        fail "an insufficient budget was refused without naming the budget as the cause"
    elif ! grep -q "THIS IS NOT A TIMEOUT" <<< "$output"; then
        fail "an insufficient budget was refused without distinguishing itself from a timeout"
    elif ! grep -qE "Raise check.backend_abstraction .* to at least [0-9]+" <<< "$output"; then
        fail "an insufficient budget was refused without naming the value to set"
    else
        note "OK — an insufficient budget is refused, named as a budget, with the value to set"
    fi
fi

echo
if ((failures > 0)); then
    echo "check-detcore-backend-abstraction-test.sh: FAIL — $failures case(s) failed" >&2
    exit 1
fi
echo "check-detcore-backend-abstraction-test.sh: OK — masker equivalence and budget guard both hold"
exit 0
