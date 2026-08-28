#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# check-script-sigpipe.sh — regression guard for shared SIGPIPE handling, and
# the only gate that compiles Hermit's rust-scripts at all.
#
# Every standalone Hermit rust-script calls `rust_script_prelude::init` so that
# a downstream reader closing the pipe early (`prog | head`) terminates the
# producer cleanly instead of panicking or exiting 141 (which would fail any
# `set -o pipefail` pipeline). This guard compiles a tiny fixture with plain
# `rustc`, asserts the pipeline is clean, and requires every tracked rust-script
# entrypoint to use `rust-script --force`. The forced Cargo check is cheap on a
# warm build and makes Cargo, rather than a separate cache-key protocol, track
# included modules and local path dependencies.
#
# ⚠️ IT THEN COMPILES EVERY ONE OF THEM, AND THAT IS NOT DECORATION.
# rust-scripts are NOT Cargo workspace members, so `cargo build`, `cargo test`,
# `cargo clippy` and `cargo fmt` never compile them — not by policy but by
# construction. Several of them pull workspace source across that boundary with
# `#[path]`:
#
#   hermit-cli/src/canonical_verdict.rs       -> ci/compat-envelope/scorecard.rs
#                                                ci/compat-envelope/pressure-test.rs
#   ci/manifest-plan/src/manifest_value.rs    -> scripts/manifest-to-commands.rs
#                                                tests/manifest-cli.rs
#   scripts/lib/*.rs                          -> all of them
#
# So a type change in a fully-gated workspace file can break a consumer that no
# cargo gate compiles, and every cargo gate will report success while it is
# broken. That is not hypothetical: on 2026-08-20 a field type change in
# canonical_verdict.rs landed with 17/17 lib tests, `cargo fmt --check` clean and
# `cargo clippy --all-targets` warning-free, having already broken scorecard.rs,
# and it blocked EVERY validate until a follow-up fixed it.
#
# The list below is DISCOVERED, never hardcoded: it is exactly the tracked files
# carrying the rust-script shebang, so a newly added script is covered without
# anyone remembering to register it. Compiling via `--package` + `cargo check`
# rather than by running each script means nothing is executed and no script's
# side effects fire.
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

fixture="scripts/lib/tests/sigpipe_smoke.rs"
[[ -f $fixture ]] || { echo "check-script-sigpipe.sh: missing $fixture" >&2; exit 2; }
command -v rustc >/dev/null 2>&1 || { echo "check-script-sigpipe.sh: rustc is required" >&2; exit 2; }
command -v realpath >/dev/null 2>&1 || { echo "check-script-sigpipe.sh: realpath is required" >&2; exit 2; }
command -v cargo >/dev/null 2>&1 || { echo "check-script-sigpipe.sh: cargo is required" >&2; exit 2; }

# Classify the CHECKOUT's submodule state, printing one of:
#   clean         every submodule is present and at its recorded revision
#   unpopulated:… one or more submodules were never checked out
#   wrongrev:…    present, but at a revision other than the recorded gitlink
#
# ⚠️ THREE STATES, NOT TWO, AND THEY NEED THREE DIFFERENT ACTIONS. This gate
# already learned once that "does not compile" is a claim about the CODE while an
# unresolvable path dependency is a claim about the CHECKOUT (4c92c6b567). The
# neighbouring case was still collapsed: a submodule that is POPULATED BUT AT THE
# WRONG REVISION produces perfectly ordinary compile errors -- `struct
# dagrun::Step has no field named jobs_env` -- so it fell through to "does not
# compile" and read as a product red. Not hypothetical: it cost the author of the
# earlier fix a diagnosis cycle on this very script, while knowing about the trap.
#
# ⚠️ AND IT DOES NOT USE `git submodule status`, WHICH LIES IN A LINKED WORKTREE.
# Measured here: with agent-utils populated and checked out at 8b0e2c0f, that
# command reported `-3ccfd127 agent-utils` -- the '-' meaning NOT CHECKED OUT,
# for a directory that is plainly checked out, and quoting the recorded revision
# rather than the actual one. Classifying on that prefix would have reported
# every wrong-rev submodule as unpopulated and sent the reader to the wrong
# remedy, which is the same defect this function exists to remove. So the state
# is derived from three direct facts instead: the recorded gitlink from the
# index, whether the directory has contents, and the submodule's own HEAD.
# Takes the generated package manifest, and considers ONLY the submodules that
# manifest actually depends on.
#
# ⚠️ THIS SCOPING IS WHAT STOPS THE FIX BECOMING THE SAME DEFECT INVERTED. This
# checkout normally has third-party/rr unpopulated, which is fine and expected.
# An unscoped check would blame it for ANY compile failure in ANY script and turn
# every genuine red into "check your submodule" -- trading one misleading claim
# for another. rust-script writes the path dependency into the generated
# manifest, so a script that needs agent-utils names it there and a script that
# does not, does not.
submodule_diagnosis() {
    local manifest=$1
    local unpop='' wrong='' path recorded actual
    while IFS= read -r path; do
        [[ -n $path ]] || continue
        grep -q "/$path/" "$manifest" 2>/dev/null || continue
        recorded=$(git ls-tree HEAD "$path" 2>/dev/null | awk '{print $3}')
        [[ -n $recorded ]] || continue
        if [[ -z $(ls -A "$path" 2>/dev/null) ]]; then
            unpop+=" $path"
            continue
        fi
        actual=$(git -C "$path" rev-parse HEAD 2>/dev/null)
        if [[ -n $actual && $actual != "$recorded" ]]; then
            wrong+=" $path"
        fi
    done < <(git ls-files --stage | awk '$1 == "160000" {print $4}')
    if [[ -n $unpop ]]; then
        printf 'unpopulated:%s' "$unpop"
    elif [[ -n $wrong ]]; then
        printf 'wrongrev:%s' "$wrong"
    else
        printf 'clean'
    fi
}


command -v rust-script >/dev/null 2>&1 || { echo "check-script-sigpipe.sh: rust-script is required (cargo install rust-script)" >&2; exit 2; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
bin="$tmp/sigpipe_smoke"

RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}" rustc --edition=2021 -O "$fixture" -o "$bin"

# The producer writes 1,000,000 lines; `head -n1` closes the pipe after one.
# Under `pipefail` the pipeline status is the producer's unless it exits 0.
status=0
out="$(set -o pipefail; "$bin" 2>"$tmp/err" | head -n1)" || status=$?

if [[ $status -ne 0 ]]; then
    echo "check-script-sigpipe.sh: FAIL — 'sigpipe_smoke | head' exited $status (want 0)" >&2
    echo "  (a SIGPIPE from an early consumer must be a clean exit, not 141/panic)" >&2
    echo "--- stderr ---" >&2
    cat "$tmp/err" >&2 || true
    exit 1
fi

if grep -qiE 'panic|Broken pipe|backtrace' "$tmp/err"; then
    echo "check-script-sigpipe.sh: FAIL — producer emitted a panic/EPIPE error on stderr:" >&2
    cat "$tmp/err" >&2
    exit 1
fi

if [[ $out != "line 0" ]]; then
    echo "check-script-sigpipe.sh: FAIL — unexpected first line: '$out' (want 'line 0')" >&2
    exit 1
fi

echo "check-script-sigpipe.sh: OK — SIGPIPE from an early consumer exits cleanly (0)"

tracked="$(git ls-files -- '*.rs')" || {
    echo "check-script-sigpipe.sh: cannot enumerate tracked Rust sources" >&2
    exit 2
}
consumers=0
entrypoints=()
while IFS= read -r source; do
    [[ -n $source ]] || continue
    IFS= read -r first <"$source" || {
        echo "check-script-sigpipe.sh: cannot read $source" >&2
        exit 2
    }
    case "$first" in
        '#!/usr/bin/env -S rust-script --force')
            [[ $(grep -Ec '^[[:space:]]*mod[[:space:]]+rust_script_prelude[[:space:]]*;' "$source") == 1 &&
               $(awk '
                   $0 ~ /^[[:space:]]*fn[[:space:]]+main\(\)([[:space:]]*->[[:space:]]*[^{}]+)?[[:space:]]*\{[[:space:]]*$/ { mains++ }
                   previous ~ /^[[:space:]]*fn[[:space:]]+main\(\)([[:space:]]*->[[:space:]]*[^{}]+)?[[:space:]]*\{[[:space:]]*$/ &&
                       $0 ~ /^[[:space:]]*rust_script_prelude::init\(\);[[:space:]]*$/ { count++ }
                   { previous = $0 }
                   END { print (mains + 0) ":" (count + 0) }
               ' "$source") == "1:1" ]] || {
                echo "check-script-sigpipe.sh: $source must have one main and initialize rust_script_prelude as its first statement" >&2
                exit 1
            }
            path_line="$(grep -B1 -m1 -E '^[[:space:]]*mod[[:space:]]+rust_script_prelude[[:space:]]*;' "$source" | head -n1)"
            prelude_relative="$(printf '%s\n' "$path_line" | sed -n 's/^[[:space:]]*#\[path = "\([^"]*\)"\][[:space:]]*$/\1/p')"
            if [[ -z $prelude_relative ||
                  $(realpath -- "$(dirname -- "$source")/$prelude_relative") != "$ROOT_DIR/scripts/lib/rust_script_prelude.rs" ]]; then
                echo "check-script-sigpipe.sh: $source must bind rust_script_prelude to scripts/lib/rust_script_prelude.rs" >&2
                exit 1
            fi
            consumers=$((consumers + 1))
            entrypoints+=("$source")
            ;;
        *rust-script*)
            echo "check-script-sigpipe.sh: $source can reuse stale code: $first" >&2
            echo "  Use: #!/usr/bin/env -S rust-script --force" >&2
            exit 1
            ;;
    esac
done <<<"$tracked"
((consumers > 0)) || {
    echo "check-script-sigpipe.sh: no tracked rust-script entrypoints found" >&2
    exit 2
}
echo "check-script-sigpipe.sh: OK — $consumers tracked rust-script entrypoint(s) force Cargo freshness"

# Compile every discovered entrypoint. Report ALL failures rather than stopping
# at the first, so one run names everything a workspace type change broke
# instead of forcing a fix-one-rerun loop.
# CLIPPY, NOT JUST `cargo check`, AND WHY IT LIVES HERE.
#
# `cargo clippy --workspace` cannot see these files: rust-scripts are not
# workspace members, so cargo never compiles them and every workspace lint gate
# reports success while they rot. This loop already DISCOVERS them by shebang
# and already builds each one, so linting them is a change of verb, not a new
# gate and not a second list to maintain. A script added tomorrow is covered the
# day it lands, with nothing to update.
#
# THE WAIVERS ARE LISTED ONCE, HERE, RATHER THAN AS `#![allow]` SCATTERED
# THROUGH FIFTEEN FILES, so the whole exemption surface is one reviewable block.
#
#   The three doc lints govern how rustdoc RENDERS a list. Nothing renders the
#   rustdoc of a standalone script, and these files use hand-aligned doc blocks
#   whose continuation lines are indented to line up under the item text --
#   which reads better in a terminal than what the lint wants and is the form
#   the authors chose deliberately. Enforcing them would reflow prose for a
#   rendering nobody performs.
#
#   `too_many_arguments` and `type_complexity` are structural. Both fire on
#   tooling functions that genuinely take many parameters -- and silencing them
#   by bundling arguments into a struct purely to satisfy a count would be
#   change for the lint's benefit rather than the reader's.
#
# Everything else is DENIED. This list should shrink, never grow: a new entry
# means a lint was switched off rather than answered.
CLIPPY_WAIVERS=(
    -A clippy::doc_overindented_list_items
    -A clippy::doc_lazy_continuation
    -A clippy::empty_line_after_doc_comments
    -A clippy::too_many_arguments
    -A clippy::type_complexity
)

# ⚠️ CLIPPY ITSELF IS A PREREQUISITE, AND ITS ABSENCE IS NOT A COMPILE FAILURE.
# This gate compiles every rust-script with `cargo clippy`. When the component is
# not installed for the active toolchain, cargo answers
# "'cargo-clippy' is not installed for the toolchain 'nightly-...'" for EVERY
# script, and the loop below reported all fifteen as "does not compile" -- a
# claim about the code, when nothing had been compiled at all. That is exactly
# the confusion the submodule branch further down was written to end, and it
# recurred here for a different missing prerequisite: measured on GitHub run
# 32814930018, where the preflight job installed rustfmt but not clippy.
#
# Checked ONCE, before the loop, because the answer cannot differ per script.
# Fails CLOSED for the same reason the submodule branch does: a toolchain
# without clippy has not established that these scripts are clean.
if ! cargo clippy -V >"$tmp/clippy.err" 2>&1; then
    echo "check-script-sigpipe.sh: REFUSED — \`cargo clippy\` is unavailable in this" >&2
    echo "  toolchain, so NOT ONE script has been compiled." >&2
    echo "  THIS IS NOT A COMPILE FAILURE and says nothing about any script." >&2
    echo "  Install the component and re-run this gate:" >&2
    echo "      rustup component add clippy" >&2
    cat "$tmp/clippy.err" >&2
    exit 2
fi

broken=()
for source in "${entrypoints[@]}"; do
    package="$(rust-script --package "$source" 2>"$tmp/pkg.err" | tail -n1)"
    if [[ -z $package || ! -f $package/Cargo.toml ]]; then
        echo "check-script-sigpipe.sh: cannot generate a Cargo package for $source" >&2
        cat "$tmp/pkg.err" >&2 || true
        broken+=("$source")
        continue
    fi
    if ! cargo clippy --manifest-path "$package/Cargo.toml" \
            -- -D warnings "${CLIPPY_WAIVERS[@]}" >"$tmp/check.out" 2>&1; then
        # ⚠️ SAY WHICH FAILURE THIS IS. "does not compile" is a claim about the
        # CODE; a path dependency Cargo cannot resolve is a claim about the
        # CHECKOUT. Conflating them made this gate report, confidently and by
        # name, that ci/compat-envelope/pressure-test.rs and scripts/validate.rs
        # do not compile -- when both compile fine and the real cause was an
        # unpopulated `agent-utils` submodule in a fresh worktree. That reading
        # cost two separate diagnosis cycles and was published as a false
        # "main is red" claim before anyone checked the underlying cargo error.
        #
        # The two need OPPOSITE actions from the reader: fix the script, versus
        # `git submodule update --init`. A message that cannot tell them apart
        # sends every reader down the wrong one.
        if grep -qE 'failed to load source for dependency|unable to update .*/(agent-utils|rs)/' \
                "$tmp/check.out"; then
            missing_dep=$(grep -oE 'unable to update [^[:space:]]+' "$tmp/check.out" |
                head -n1 | sed 's/^unable to update //')
            echo "check-script-sigpipe.sh: REFUSED — cannot build $source because a path" >&2
            echo "  dependency could not be resolved: ${missing_dep:-<see cargo output below>}" >&2
            echo "  THIS IS NOT A COMPILE FAILURE and says nothing about the script." >&2
            # No longer a guess: ask the checkout directly. The hedge existed
            # only because the earlier version had no way to tell.
            resolved=$(submodule_diagnosis "$package/Cargo.toml")
            case "$resolved" in
                unpopulated:*)
                    echo "  CONFIRMED UNPOPULATED:${resolved#unpopulated:} — never checked out. Run:" >&2
                    echo "      git submodule update --init${resolved#unpopulated:}" >&2
                    ;;
                wrongrev:*)
                    echo "  The submodule is PRESENT but at the WRONG REVISION:${resolved#wrongrev:}" >&2
                    echo "      git submodule update --init${resolved#wrongrev:}" >&2
                    ;;
                *)
                    echo "  Submodules look correct, so this is a path-dependency problem of" >&2
                    echo "  another kind — read the cargo output below rather than assuming." >&2
                    ;;
            esac
            echo "  and re-run this gate. Reporting it as 'does not compile' previously" >&2
            echo "  produced a false main-red claim about two scripts that build fine." >&2
            grep -E '^(error|Caused by:)' -A2 "$tmp/check.out" >&2 || cat "$tmp/check.out" >&2
            # Fail CLOSED: an unbuildable checkout has not established that these
            # scripts are clean, so this must not exit 0. It simply must not lie
            # about why.
            exit 2
        fi
        # ⚠️ BEFORE CALLING IT A COMPILE FAILURE, ASK WHETHER THE CHECKOUT CAN
        # SUPPORT THE CLAIM. A submodule at the WRONG REVISION produces ordinary
        # compile errors -- missing struct fields, unknown methods -- that look
        # exactly like a broken script. The reader needs `git submodule update`,
        # not a code fix, and nothing in cargo's output says so.
        diagnosis=$(submodule_diagnosis "$package/Cargo.toml")
        case "$diagnosis" in
            unpopulated:*)
                echo "check-script-sigpipe.sh: REFUSED — cannot judge $source: submodule(s) UNPOPULATED:${diagnosis#unpopulated:}" >&2
                echo "  THIS IS NOT A COMPILE FAILURE and says nothing about the script." >&2
                echo "  The submodule was never checked out. Run:" >&2
                echo "      git submodule update --init${diagnosis#unpopulated:}" >&2
                exit 2
                ;;
            wrongrev:*)
                echo "check-script-sigpipe.sh: REFUSED — cannot judge $source: submodule(s) at the WRONG REVISION:${diagnosis#wrongrev:}" >&2
                echo "  THIS IS NOT A COMPILE FAILURE and says nothing about the script." >&2
                echo "  The submodule IS present, but at a revision other than the one this" >&2
                echo "  commit records, so its API does not match what the script expects." >&2
                echo "  That produces ordinary-looking compile errors. Recorded vs checked out:" >&2
                for sm in ${diagnosis#wrongrev:}; do
                    echo "      $sm  recorded=$(git ls-tree HEAD "$sm" | awk '{print $3}')  checked-out=$(git -C "$sm" rev-parse HEAD 2>/dev/null)" >&2
                done
                echo "  Run:" >&2
                echo "      git submodule update --init${diagnosis#wrongrev:}" >&2
                exit 2
                ;;
        esac
        # Submodules are clean, so a compile failure IS a claim about the code.
        echo "check-script-sigpipe.sh: FAIL — $source does not compile cleanly under clippy" >&2
        grep -E '^(error|warning: unused)' -A4 "$tmp/check.out" >&2 || cat "$tmp/check.out" >&2
        broken+=("$source")
    fi
done
if ((${#broken[@]} > 0)); then
    echo "check-script-sigpipe.sh: FAIL — ${#broken[@]} of $consumers rust-script(s) do not compile:" >&2
    printf '  %s\n' "${broken[@]}" >&2
    echo "  No cargo gate compiles these, so cargo build/test/clippy/fmt will all still pass." >&2
    exit 1
fi
echo "check-script-sigpipe.sh: OK — all $consumers rust-script entrypoint(s) compile and are clippy-clean"
