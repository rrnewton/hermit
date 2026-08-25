#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# check-detcore-backend-abstraction.sh
# ------------------------------------
# Enforce the DETCORE BACKEND-ABSTRACTION COMMANDMENT.
#
# Commandment (see detcore/Cargo.toml and detcore/src/lib.rs):
#
#   The detcore core library depends ONLY on the abstract Reverie interface
#   crate (`reverie`, whose package is `reverie-core`). It MUST NEVER depend on
#   another `reverie-*` crate. Backends are selected and instantiated
#   EXCLUSIVELY by the `hermit-cli` package, which constructs a detcore tool and
#   runs it against a chosen backend. There are no backend-specific hacks in
#   detcore.
#
# Why: Hermit follows Reverie's abstract instrumentation model. A backend
# dependency in detcore would couple the determinism engine to one tracing
# mechanism and break the clean abstraction boundary.
#
# What this lint checks:
#   1. detcore/Cargo.toml: no non-core Reverie crate appears in any NON-test
#      dependency table ([dependencies], [build-dependencies],
#      [target.*.dependencies]).
#   2. detcore/src/**: no non-core Reverie crate is imported or referenced from
#      the library source (use / extern crate / path `reverie_ptrace::` etc.).
#
# What this lint intentionally ALLOWS:
#   - Backend crates under [dev-dependencies] and in detcore/tests/**. Detcore's
#     own integration tests must drive a real tracer to exercise the tool; that
#     test-only coupling does not leak into the shipped `detcore` rlib or its
#     consumers.
#
# Exit codes:
#   0  boundary intact
#   1  violation detected (backend dep or import in the core library)
#   2  usage / environment error

set -uo pipefail

repo_root_override=""
skip_negative_control=false
while (($# > 0)); do
    case "$1" in
        --repo-root)
            if (($# < 2)); then
                echo "error: --repo-root requires a path" >&2
                exit 2
            fi
            repo_root_override=$2
            shift 2
            ;;
        --skip-negative-control)
            skip_negative_control=true
            shift
            ;;
        -h|--help)
            echo "usage: $0 [--repo-root PATH] [--skip-negative-control]"
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

# --- output helpers ----------------------------------------------------------

is_tty() { [[ -t 1 ]]; }
if is_tty; then
    C_RED=$'\033[31m'; C_GRN=$'\033[32m'; C_DIM=$'\033[2m'; C_RST=$'\033[0m'
else
    C_RED=""; C_GRN=""; C_DIM=""; C_RST=""
fi
info() { echo "${C_DIM}info:${C_RST} $*"; }
ok()   { echo "${C_GRN}ok:${C_RST}   $*"; }
err()  { echo "${C_RED}error:${C_RST} $*" >&2; }

# --- locate the repo and detcore ---------------------------------------------

script_dir() { cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd; }
if [[ -n $repo_root_override ]]; then
    REPO_ROOT="$(cd -- "$repo_root_override" && pwd)"
else
    REPO_ROOT="$(cd -- "$(script_dir)/.." && pwd)"
fi

readonly DETCORE_MANIFEST="$REPO_ROOT/detcore/Cargo.toml"
readonly DETCORE_SRC="$REPO_ROOT/detcore/src"

if [[ ! -f $DETCORE_MANIFEST ]]; then
    err "detcore manifest not found: $DETCORE_MANIFEST"
    exit 2
fi
if [[ ! -d $DETCORE_SRC ]]; then
    err "detcore source directory not found: $DETCORE_SRC"
    exit 2
fi

# Derive every non-core Reverie crate named by a workspace member. This set is
# intentionally broader than today's execution backends: the commandment says
# detcore depends only on reverie-core, so a direct dependency on any other
# Reverie implementation/support crate is a boundary violation. A new
# reverie-* dependency automatically joins the prohibited set without editing
# this lint.
backend_output="$(python3 - "$REPO_ROOT" <<'PY'
import glob
import sys
import tomllib
from pathlib import Path

root = Path(sys.argv[1])

def load(path: Path):
    with path.open("rb") as source:
        return tomllib.load(source)

root_manifest = load(root / "Cargo.toml")
members = root_manifest.get("workspace", {}).get("members", [])
manifest_paths = {root / "Cargo.toml"}
for pattern in members:
    for candidate_name in glob.glob(str(root / pattern)):
        candidate = Path(candidate_name)
        manifest = candidate if candidate.name == "Cargo.toml" else candidate / "Cargo.toml"
        if manifest.is_file():
            manifest_paths.add(manifest)

def dependency_tables(document):
    for key in ("dependencies", "dev-dependencies", "build-dependencies"):
        table = document.get(key)
        if isinstance(table, dict):
            yield table
    workspace = document.get("workspace")
    if isinstance(workspace, dict):
        table = workspace.get("dependencies")
        if isinstance(table, dict):
            yield table
    target = document.get("target")
    if isinstance(target, dict):
        for target_table in target.values():
            if not isinstance(target_table, dict):
                continue
            for key in ("dependencies", "dev-dependencies", "build-dependencies"):
                table = target_table.get(key)
                if isinstance(table, dict):
                    yield table

crates = set()
for manifest in manifest_paths:
    document = load(manifest)
    for table in dependency_tables(document):
        for dependency_key, specification in table.items():
            package = dependency_key
            if isinstance(specification, dict):
                package = specification.get("package", dependency_key)
            if package.startswith("reverie-") and package != "reverie-core":
                crates.add(package)

print("\n".join(sorted(crates)))
PY
)" || {
    err "failed to derive non-core Reverie crates from workspace Cargo manifests"
    exit 2
}

if [[ -z $backend_output ]]; then
    err "workspace declares no non-core reverie-* crates; refusing a vacuous abstraction check"
    exit 2
fi
mapfile -t BACKEND_CRATES <<< "$backend_output"
info "derived prohibited Reverie crates from workspace: ${BACKEND_CRATES[*]}"

violations=0

# --- 1. manifest: no backend in non-test dependency tables -------------------
#
# Walk Cargo.toml tracking the current table header. A dependency line is
# flagged when its key (or a `package = "..."` rename target) names a backend
# crate AND the current table is a dependency table that is not
# [dev-dependencies]. Commented lines are ignored.

manifest_hits="$(
    awk -v backends="${BACKEND_CRATES[*]}" '
        function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s }
        BEGIN {
            n = split(backends, arr, " ")
            for (i = 1; i <= n; i++) banned[arr[i]] = 1
            insec = 0
        }
        # Table header line, e.g. [dependencies] or a target-scoped dep table.
        /^[ \t]*\[/ {
            hdr = trim($0)
            # A dependency table, but NOT the test-only dev-dependencies table.
            insec = (hdr ~ /dependencies\][ \t]*$/ && hdr !~ /dev-dependencies/)
            next
        }
        # Skip blank and comment lines.
        /^[ \t]*#/ { next }
        /^[ \t]*$/ { next }
        insec {
            key = $0; sub(/=.*/, "", key); key = trim(key)
            # strip optional surrounding quotes from the dependency key
            gsub(/"/, "", key)
            pkg = ""
            if (match($0, /package[ \t]*=[ \t]*"[^"]+"/)) {
                pkg = substr($0, RSTART, RLENGTH)
                sub(/.*package[ \t]*=[ \t]*"/, "", pkg); sub(/".*/, "", pkg)
            }
            if ((key in banned) || (pkg != "" && (pkg in banned)))
                printf "  %d: %s\n", FNR, trim($0)
        }
    ' "$DETCORE_MANIFEST"
)"

if [[ -n $manifest_hits ]]; then
    err "detcore/Cargo.toml declares a non-core Reverie crate in a non-test dependency table:"
    printf '%s\n' "$manifest_hits" >&2
    err "detcore must depend only on the abstract 'reverie' crate. Move backend wiring to hermit-cli."
    ((violations++))
else
    ok "detcore/Cargo.toml: no non-core Reverie crate in runtime/build dependency tables"
fi

# --- 2. library source: no backend imports -----------------------------------

src_hits="$(python3 - "$DETCORE_SRC" "${BACKEND_CRATES[@]}" <<'PY'
import re
import sys
from pathlib import Path

source_root = Path(sys.argv[1])
backend_modules = [crate.replace("-", "_") for crate in sys.argv[2:]]
module_pattern = "|".join(re.escape(module) for module in backend_modules)
identifier = rf"(?<![A-Za-z0-9_])(?:{module_pattern})(?![A-Za-z0-9_])"
path_reference = re.compile(identifier + r"\s*::")
extern_crate = re.compile(r"\bextern\s+crate\s+(?:" + module_pattern + r")\b")
use_statement = re.compile(r"\buse\b(?P<body>[^;]*);", re.DOTALL)
module_name = re.compile(identifier)


# ⚠️ THE STATE MACHINE BELOW IS UNCHANGED. Only the way it WALKS the source is
# different, and that distinction is the whole point of this note.
#
# The previous version built `list(source)` and stepped one character at a time
# in Python, blanking each masked character individually. That is O(bytes) of
# interpreter work per invocation -- 10.84s for detcore/src's 49 files, measured
# 2026-08-25 -- and this script re-invokes ITSELF once per negative control, so
# the cost was bytes x controls. Both factors grow: detcore grows, and the
# control list is DERIVED from the workspace's Reverie crates. Against the
# node's fixed 120s budget that product crossed the ceiling and the node was
# killed at 120.030s with every assertion passing.
#
# Ordinary code cannot begin a comment, string, char literal or raw string, so
# the scan now jumps to the next position where the state machine could
# actually change state and blanks whole spans with one C-level substitution.
# Every transition rule, terminator search and fall-through below is preserved
# verbatim.
#
# Equivalence is TESTED, not asserted -- see check-detcore-backend-abstraction-test.sh:
# byte-identical output on all 541 tracked .rs files (10.2 MB), on 33 hand-written
# adversarial cases (nested block comments, raw strings with hash counts,
# unterminated constructs, lifetimes vs char literals), and on 40,000 fuzz
# inputs drawn from an alphabet of masking metacharacters.
TRIGGER = re.compile(r'(?<![A-Za-z0-9_])(?:br|cr|r)#{0,255}"|[/"\']')
NON_NEWLINE = re.compile(r"[^\n]")


def blank_span(source, start, end):
    """Blank a span, PRESERVING NEWLINES so reported line numbers stay correct."""
    return NON_NEWLINE.sub(" ", source[start:end])


def mask_comments_and_literals(source):
    length = len(source)
    pieces = []
    copied = 0
    index = 0
    while index < length:
        hit = TRIGGER.search(source, index)
        if hit is None:
            break
        index = hit.start()

        def emit(end):
            pieces.append(source[copied:index])
            pieces.append(blank_span(source, index, end))
            return end

        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            if end == -1:
                end = length
            copied = index = emit(end)
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
            copied = index = emit(end)
            continue

        raw = re.match(r'(?:br|cr|r)(?P<hashes>#{0,255})"', source[index:])
        if raw and (index == 0 or not (source[index - 1].isalnum() or source[index - 1] == "_")):
            terminator = '"' + raw.group("hashes")
            end = source.find(terminator, index + raw.end())
            end = length if end == -1 else end + len(terminator)
            copied = index = emit(end)
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
            copied = index = emit(end)
            continue

        if source[index] == "'" and index + 2 < length:
            if source[index + 1] == "\\":
                end = index + 2
                while end < length and source[end] != "\n":
                    if source[end] == "'" and source[end - 1] != "\\":
                        end += 1
                        copied = index = emit(end)
                        break
                    end += 1
                else:
                    index += 1
                continue
            if source[index + 2] == "'":
                copied = index = emit(index + 3)
                continue

        index += 1
    pieces.append(source[copied:])
    return "".join(pieces)


for path in sorted(source_root.rglob("*.rs")):
    source = path.read_text(encoding="utf-8")
    code = mask_comments_and_literals(source)
    hit_offsets = {match.start() for match in path_reference.finditer(code)}
    hit_offsets.update(match.start() for match in extern_crate.finditer(code))
    for statement in use_statement.finditer(code):
        hit_offsets.update(
            statement.start("body") + match.start()
            for match in module_name.finditer(statement.group("body"))
        )
    lines = source.splitlines()
    for offset in sorted(hit_offsets):
        line_number = code.count("\n", 0, offset) + 1
        print(f"{path}:{line_number}:{lines[line_number - 1]}")
PY
)" || {
    err "failed to scan detcore Rust source"
    exit 2
}

if [[ -n $src_hits ]]; then
    err "detcore/src references a non-core Reverie crate module:"
    printf '%s\n' "$src_hits" >&2
    err "detcore library code must use only the abstract 'reverie' interfaces."
    ((violations++))
else
    ok "detcore/src: no imports from derived non-core Reverie crates"
fi

# Exercise the real checker against scratch detcore copies. This is a negative
# control, not a mock: each recursive invocation re-derives its prohibited set
# from the scratch workspace after the planted dependency is added.
# ⚠️ THE WORK IS DERIVED, SO THE BUDGET MUST BE DERIVED FROM THE SAME LIST.
#
# This node's cost is one self-invocation per negative control, and the control
# list comes from BACKEND_CRATES, which is DERIVED from the workspace's Reverie
# crates. The DAG declares a single constant wall timeout. On 2026-08-25 that
# constant was 120s and the node was killed at 120.030s with every assertion
# passing -- not a correctness failure, a budget that had silently been outgrown.
#
# Raising the constant would have fixed that day and broken again at the next
# Reverie crate, because a constant cannot track a derived quantity. So the
# REQUIREMENT is computed here from the same derived list that drives the work,
# and the declared constant is CHECKED against it. Adding a crate can no longer
# cross the ceiling silently: it either still fits, or this fails in under a
# second saying exactly which number to change and why.
#
# The runner's schema takes a literal integer for `timeout`, and agent-utils is
# a separately pinned, main-only repository, so the declared value stays a
# reviewable constant in ci/dag/portable.json. What is DERIVED is the
# requirement that constant must satisfy.
#
# Per-unit costs are measured, not guessed (2026-08-25, this checkout):
#   base pass, no controls   0.45s 0.58s 0.48s  -> ~0.50s
#   full run, 10 controls    5.57s 5.55s 5.50s  -> ~5.54s
#   therefore per control    (5.54 - 0.50) / 10  = ~0.50s
# The budgets below carry roughly 6x headroom over those measurements, so a
# slower runner or a cold cache does not trip the guard.
BASE_BUDGET_S=30
PER_CONTROL_BUDGET_S=3

check_declared_budget() {
    local dag="$REPO_ROOT/ci/dag/portable.json"
    # Only meaningful in a real checkout. Negative controls run against scratch
    # copies that contain detcore alone and execute no controls of their own.
    [[ -f $dag ]] || return 0

    local control_count=$1
    local required=$((BASE_BUDGET_S + PER_CONTROL_BUDGET_S * control_count))
    local declared
    declared="$(python3 - "$dag" <<'PYBUDGET'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    graph = json.load(handle)
for step in graph.get("steps", []):
    if step.get("group") == "check" and step.get("job") == "backend_abstraction":
        print(int(step.get("timeout", 0)))
        break
else:
    print(0)
PYBUDGET
)" || return 1

    if ((declared <= 0)); then
        err "check.backend_abstraction declares no wall timeout in ci/dag/portable.json"
        return 1
    fi
    if ((declared < required)); then
        err "check.backend_abstraction is BUDGETED FOR LESS WORK THAN IT DERIVES."
        err "  the workspace derives $control_count negative control(s)"
        err "  requiring ${required}s = ${BASE_BUDGET_S}s base + ${control_count} x ${PER_CONTROL_BUDGET_S}s"
        err "  but ci/dag/portable.json declares only ${declared}s"
        err "  THIS IS NOT A TIMEOUT AND NOTHING RAN LONG. It is the declared"
        err "  budget failing to cover work the workspace now derives."
        err "  Raise check.backend_abstraction \"timeout\" to at least ${required}, with evidence."
        return 1
    fi
    info "budget: ${declared}s declared covers $control_count derived control(s) needing ~${required}s"
    return 0
}

run_negative_controls() {
    local scratch backend module output status
    local -a control_crates=("${BACKEND_CRATES[@]}")

    # e9patch is not currently declared by a workspace member, so it cannot be
    # part of the derived set yet. Keep it as a sentinel until it is declared;
    # once present, the derived list supplies it and this append is skipped.
    if [[ " ${control_crates[*]} " != *" reverie-e9patch "* ]]; then
        control_crates+=(reverie-e9patch)
    fi

    # +2: the source-path control and the literal/comment control below each
    # cost one further self-invocation, exactly like a crate control.
    if ! check_declared_budget $((${#control_crates[@]} + 2)); then
        return 1
    fi

    for backend in "${control_crates[@]}"; do
        if ! scratch=$(mktemp -d); then
            err "negative control could not create a scratch directory"
            return 1
        fi
        if ! cp -a "$REPO_ROOT/detcore" "$scratch/detcore" ||
           ! printf '[workspace]\nmembers = ["detcore"]\nresolver = "2"\n' \
                > "$scratch/Cargo.toml" ||
           ! printf '\n[target.'"'"'cfg(any())'"'"'.dependencies]\n%s = "0.2.0"\n' \
                "$backend" >> "$scratch/detcore/Cargo.toml"; then
            err "negative control could not prepare the $backend scratch copy"
            rm -rf -- "$scratch"
            return 1
        fi

        output="$("${BASH_SOURCE[0]}" --repo-root "$scratch" \
            --skip-negative-control 2>&1)"
        status=$?
        rm -rf -- "$scratch"

        if ((status != 1)); then
            err "negative control for $backend returned $status, expected 1"
            printf '%s\n' "$output" >&2
            return 1
        fi
        if ! grep -Fq "$backend" <<< "$output"; then
            err "negative control failed without identifying $backend"
            printf '%s\n' "$output" >&2
            return 1
        fi
        ok "negative control: planted $backend dependency was rejected"
    done

    backend=${BACKEND_CRATES[0]}
    module=${backend//-/_}

    if ! scratch=$(mktemp -d); then
        err "source negative control could not create a scratch directory"
        return 1
    fi
    if ! cp -a "$REPO_ROOT/detcore" "$scratch/detcore" ||
       ! printf '[workspace]\nmembers = ["detcore"]\nresolver = "2"\n' \
            > "$scratch/Cargo.toml" ||
       ! printf '\n[target.'"'"'cfg(any())'"'"'.dev-dependencies]\n%s = "0.2.0"\n' \
            "$backend" >> "$scratch/detcore/Cargo.toml" ||
       ! printf 'extern crate %s;\nuse %s::CheckpointNegativeControl;\nfn source_path() { %s::checkpoint_negative_control(); }\n' \
            "$module" "$module" "$module" \
            > "$scratch/detcore/src/backend_abstraction_negative_control.rs"; then
        err "source negative control could not prepare the scratch copy"
        rm -rf -- "$scratch"
        return 1
    fi

    output="$("${BASH_SOURCE[0]}" --repo-root "$scratch" \
        --skip-negative-control 2>&1)"
    status=$?
    rm -rf -- "$scratch"

    if ((status != 1)); then
        err "source negative control returned $status, expected 1"
        printf '%s\n' "$output" >&2
        return 1
    fi
    for expected in \
        "backend_abstraction_negative_control.rs:1:extern crate $module;" \
        "backend_abstraction_negative_control.rs:2:use $module::" \
        "backend_abstraction_negative_control.rs:3:fn source_path() { $module::"; do
        if ! grep -Fq "$expected" <<< "$output"; then
            err "source negative control failed without identifying: $expected"
            printf '%s\n' "$output" >&2
            return 1
        fi
    done
    ok "negative control: planted $module extern, import, and source path were rejected"

    if ! scratch=$(mktemp -d); then
        err "literal/comment control could not create a scratch directory"
        return 1
    fi
    if ! cp -a "$REPO_ROOT/detcore" "$scratch/detcore" ||
       ! printf '[workspace]\nmembers = ["detcore"]\nresolver = "2"\n' \
            > "$scratch/Cargo.toml" ||
       ! printf '\n[target.'"'"'cfg(any())'"'"'.dev-dependencies]\n%s = "0.2.0"\n' \
            "$backend" >> "$scratch/detcore/Cargo.toml" ||
       ! printf 'const NORMAL: &str = "%s::evidence";\nconst RAW: &str = r#"%s::evidence"#;\n// use %s::Comment;\n/* %s::Comment */\n' \
            "$module" "$module" "$module" "$module" \
            > "$scratch/detcore/src/backend_abstraction_literal_control.rs"; then
        err "literal/comment control could not prepare the scratch copy"
        rm -rf -- "$scratch"
        return 1
    fi

    output="$("${BASH_SOURCE[0]}" --repo-root "$scratch" \
        --skip-negative-control 2>&1)"
    status=$?
    rm -rf -- "$scratch"

    if ((status != 0)); then
        err "literal/comment control returned $status, expected 0"
        printf '%s\n' "$output" >&2
        return 1
    fi
    ok "literal/comment control: $module text outside Rust code was accepted"
}

# --- summary -----------------------------------------------------------------

echo
if ((violations > 0)); then
    err "backend-abstraction commandment VIOLATED ($violations check(s) failed)."
    err "See detcore/src/lib.rs and detcore/Cargo.toml for the commandment."
    exit 1
fi
if ! $skip_negative_control && ! run_negative_controls; then
    err "backend-abstraction negative control FAILED; the lint is not trustworthy."
    exit 2
fi
ok "backend-abstraction commandment intact: detcore depends only on abstract 'reverie'."
exit 0
