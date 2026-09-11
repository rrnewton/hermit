#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# DAG-node wrapper for test.liteinst_strict: report a missing or incomplete
# staged runtime/provenance pair as a no_result, not as a product failure.
#
# WHY THIS EXISTS. `hermit-cli/tests/common/liteinst.rs` asserts on the staged
# runtime from inside a `#[test]`, and a panic in a test is a FAILURE. So "the
# LiteInst runtime is not staged" (a SETUP condition -- fix your checkout) and a
# real product defect -- the thing these tests exist to catch -- produce the SAME
# red on this node. That second case used to be spelled "the clone boundary is
# not enforced". Since Reverie began following threads and child processes it is
# the opposite: a boundary REAPPEARING, which is what
# `liteinst_thread_clone_runs_without_sigsys` and `liteinst_fork_runs_without_hanging`
# now catch by requiring the guest to run. An unavailable runtime is therefore
# a preparation failure; this wrapper keeps that separate from a test failure.
#
# WHY 75. scripts/validate.rs reserves exactly one nonzero code that is not a
# product failure: NO_RESULT_EXIT_CODE = 75, matched by outcome_is_no_result()
# and excluded by outcome_is_failure(). Any other value is classified a FAILURE.
# ci/lint-checks-node.sh already uses 75 for precisely this shape; this follows
# that spelling rather than inventing a second one.
#
# ⚠️ WHY THIS ONE PRE-FLIGHTS WHERE lint-checks-node.sh DELIBERATELY DOES NOT.
# That node learned the hard way not to exit 75 before running its target: its
# precondition affected one arm of one case in one of seventeen checkers, so
# skipping the target threw away sixteen checkers' worth of signal. THE RATIO IS
# INVERTED HERE. Every test in this target that touches LiteInst cannot run
# without the runtime -- measured, 22 of 23 -- so running the target establishes
# nothing about the product and produces 22 reds that mean "not staged". Checking
# first is what makes the node's red mean one thing.
#
# ⚠️ AND A NO_RESULT MUST NEVER SWALLOW A RED. That is why this checks BEFORE the
# target rather than reclassifying its exit code afterwards: if the target runs,
# its verdict is passed through untouched, so there is no path by which a real
# failure becomes a no_result.

set -uo pipefail

usage() {
    cat >&2 <<'USAGE'
Usage: ci/liteinst-strict-node.sh [--self-test] -- COMMAND [ARGS...]

Verifies the staged LiteInst runtime and provenance are present, then execs
COMMAND. Exits 75 (no_result) when the pair is unavailable, so a setup
condition is not reported as a product failure.
USAGE
}

# The same two locations the session-aware runtime discovery searches, in the
# same order.
runtime_path() {
    local dir=${HERMIT_LITEINST_STAGE_DIR:-target/release}
    if [[ -f $dir/libhermit_liteinst_detcore.so ]]; then
        printf '%s' "$dir/libhermit_liteinst_detcore.so"
        return 0
    fi
    if [[ -f $dir/deps/libhermit_liteinst_detcore.so ]]; then
        printf '%s' "$dir/deps/libhermit_liteinst_detcore.so"
        return 0
    fi
    printf '%s' "$dir/libhermit_liteinst_detcore.so"
    return 1
}

# This preflight checks only the presence of both files. The product performs
# the descriptor, byte hash, source identity and pin checks before launch.
classify() {
    local path
    if ! path=$(runtime_path); then
        printf 'missing %s' "$path"
        return 0
    fi
    if [[ ! -f $path ]]; then
        printf 'missing %s' "$path"
        return 0
    fi
    if [[ ! -f "$path.provenance.json" ]]; then
        printf 'incomplete %s' "$path"
        return 0
    fi
    if [[ ! -s "$path.provenance.json" ]]; then
        printf 'incomplete %s' "$path"
        return 0
    fi
    printf 'staged %s' "$path"
    return 0
}

self_test() {
    local checks=0 failures=0 scratch
    scratch=$(mktemp -d) || return 1
    trap 'rm -rf "$scratch"' RETURN

    check() {
        local name=$1 want=$2 got=$3
        checks=$((checks + 1))
        if [[ $got == "$want" ]]; then
            printf 'ok   %s\n' "$name"
        else
            printf 'FAIL %s: expected %q, got %q\n' "$name" "$want" "$got" >&2
            failures=$((failures + 1))
        fi
    }

    # A complete pair must not be reported as unavailable.
    mkdir -p "$scratch/good"
    printf 'runtime\n' >"$scratch/good/libhermit_liteinst_detcore.so"
    printf 'provenance\n' >"$scratch/good/libhermit_liteinst_detcore.so.provenance.json"
    check 'a staged runtime/provenance pair is not a no_result' \
        "staged $scratch/good/libhermit_liteinst_detcore.so" \
        "$(HERMIT_LITEINST_STAGE_DIR=$scratch/good classify)"

    mkdir -p "$scratch/absent"
    check 'no runtime at all is a no_result' \
        "missing $scratch/absent/libhermit_liteinst_detcore.so" \
        "$(HERMIT_LITEINST_STAGE_DIR=$scratch/absent classify)"

    mkdir -p "$scratch/incomplete"
    printf 'runtime\n' >"$scratch/incomplete/libhermit_liteinst_detcore.so"
    check 'a runtime without provenance is a no_result' \
        "incomplete $scratch/incomplete/libhermit_liteinst_detcore.so" \
        "$(HERMIT_LITEINST_STAGE_DIR=$scratch/incomplete classify)"

    mkdir -p "$scratch/empty"
    printf 'runtime\n' >"$scratch/empty/libhermit_liteinst_detcore.so"
    : >"$scratch/empty/libhermit_liteinst_detcore.so.provenance.json"
    check 'empty provenance is a no_result' \
        "incomplete $scratch/empty/libhermit_liteinst_detcore.so" \
        "$(HERMIT_LITEINST_STAGE_DIR=$scratch/empty classify)"

    mkdir -p "$scratch/deps-only/deps"
    printf 'runtime\n' >"$scratch/deps-only/deps/libhermit_liteinst_detcore.so"
    printf 'provenance\n' \
        >"$scratch/deps-only/deps/libhermit_liteinst_detcore.so.provenance.json"
    check 'the deps/ fallback is searched, as the product searches it' \
        "staged $scratch/deps-only/deps/libhermit_liteinst_detcore.so" \
        "$(HERMIT_LITEINST_STAGE_DIR=$scratch/deps-only classify)"

    # The standalone producer is the documented way to stage this runtime. Use
    # a fake Cargo command so this self-test checks its file contract without
    # compiling Reverie.
    cat >"$scratch/fake-cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
set -euo pipefail
: "${HERMIT_LITEINST_STAGE:?missing HERMIT_LITEINST_STAGE}"
for variable in LD_AUDIT GCC_EXEC_PREFIX COMPILER_PATH CARGO_TARGET_DIR CARGO_BUILD_TARGET \
    CARGO_PROFILE_RELEASE_LTO HOST_CC TARGET_CXX PKG_CONFIG RUSTC RUSTC_BOOTSTRAP; do
    [[ ! -v $variable ]]
done
if [[ -n ${FAKE_CARGO_LOG:-} ]]; then
    target_dir=
    cargo_config=
    while (($#)); do
        case $1 in
            --target-dir)
                target_dir=$2
                shift 2
                ;;
            --config)
                cargo_config=$2
                shift 2
                ;;
            *)
                shift
                ;;
        esac
    done
    if [[ -n ${FAKE_EXPECT_CONFIG:-} ]]; then
        [[ $cargo_config == "$FAKE_EXPECT_CONFIG" ]]
    fi
    printf '%s\n' "$target_dir" >>"$FAKE_CARGO_LOG"
fi
printf 'self-test runtime\n' >"$HERMIT_LITEINST_STAGE"
printf 'self-test provenance\n' >"$HERMIT_LITEINST_STAGE.provenance.json"
printf 'self-test source record\n' >"$HERMIT_LITEINST_SOURCE_RECORD"
FAKE_CARGO
    chmod +x "$scratch/fake-cargo"
    mkdir -p "$scratch/producer"
    local produced="$scratch/producer/libhermit_liteinst_detcore.so"
    if CARGO="$scratch/fake-cargo" HERMIT_LITEINST_REVERIE_ROOT="$PWD" \
        LD_AUDIT= GCC_EXEC_PREFIX=unadmitted COMPILER_PATH=unadmitted \
        CARGO_TARGET_DIR=unadmitted CARGO_BUILD_TARGET=unadmitted \
        CARGO_PROFILE_RELEASE_LTO=unadmitted HOST_CC=unadmitted TARGET_CXX=unadmitted \
        PKG_CONFIG=unadmitted RUSTC=unadmitted RUSTC_BOOTSTRAP=unadmitted \
        ./scripts/stage-liteinst-runtime.sh \
        dev "$produced" "$scratch/runtime-target" \
        >"$scratch/producer.out" 2>"$scratch/producer.err"; then
        check 'the standalone producer moves the runtime/provenance pair' \
            "staged $produced" \
            "$(HERMIT_LITEINST_STAGE_DIR=$scratch/producer classify)"
        check 'the standalone producer publishes its source record' \
            'self-test source record' \
            "$(sed -n '1p' "$produced.source-record.json" 2>/dev/null || true)"
        check 'the published source record has the exact expected digest' \
            c824584005aabf5fe4b4907a794941aacd156d9934f89ea440a9d2764503a086 \
            "$(sha256sum -- "$produced.source-record.json" 2>/dev/null | cut -d ' ' -f 1)"
    else
        printf 'FAIL the standalone producer command failed\n' >&2
        cat "$scratch/producer.out" "$scratch/producer.err" >&2
        failures=$((failures + 1))
    fi

    if CARGO="$scratch/fake-cargo" HERMIT_LITEINST_REVERIE_ROOT="$PWD" \
        ./scripts/stage-liteinst-runtime.sh dev "$scratch/wrong-name.so" \
        "$scratch/wrong-name-target" >"$scratch/wrong-name.out" \
        2>"$scratch/wrong-name.err"; then
        printf 'FAIL the standalone producer accepted the legacy basename\n' >&2
        failures=$((failures + 1))
    else
        check 'the standalone producer refuses the legacy basename' \
            absent "$([[ -e $scratch/wrong-name.so ]] && printf present || printf absent)"
    fi

    mkdir -p "$scratch/interrupted"
    local interrupted="$scratch/interrupted/libhermit_liteinst_detcore.so"
    mkdir "$interrupted.provenance.json"
    if CARGO="$scratch/fake-cargo" HERMIT_LITEINST_REVERIE_ROOT="$PWD" \
        ./scripts/stage-liteinst-runtime.sh dev "$interrupted" \
        "$scratch/interrupted-target" >"$scratch/interrupted.out" \
        2>"$scratch/interrupted.err"; then
        printf 'FAIL the standalone producer reported success after the provenance rename was refused\n' >&2
        failures=$((failures + 1))
    else
        check 'an interrupted provenance rename remains incomplete' \
            "incomplete $interrupted" \
            "$(HERMIT_LITEINST_STAGE_DIR=$scratch/interrupted classify)"
    fi

    mkdir -p "$scratch/source-key" "$scratch/reverie-key"
    git -C "$scratch/source-key" init -q
    git -C "$scratch/source-key" config user.name liteinst-self-test
    git -C "$scratch/source-key" config user.email liteinst-self-test@example.invalid
    printf 'first\n' >"$scratch/source-key/input"
    git -C "$scratch/source-key" add input
    git -C "$scratch/source-key" commit -qm initial
    git -C "$scratch/reverie-key" init -q
    git -C "$scratch/reverie-key" config user.name liteinst-self-test
    git -C "$scratch/reverie-key" config user.email liteinst-self-test@example.invalid
    printf 'first\n' >"$scratch/reverie-key/input"
    git -C "$scratch/reverie-key" add input
    git -C "$scratch/reverie-key" commit -qm initial
    printf '[net]\noffline = true\n' >"$scratch/cargo-config.toml"
    : >"$scratch/target-log"
    FAKE_CARGO_LOG="$scratch/target-log" \
        FAKE_EXPECT_CONFIG="$scratch/cargo-config.toml" CARGO="$scratch/fake-cargo" \
        HERMIT_LITEINST_HERMIT_ROOT="$scratch/source-key" \
        HERMIT_LITEINST_REVERIE_ROOT="$scratch/reverie-key" \
        HERMIT_LITEINST_CARGO_CONFIG="$scratch/cargo-config.toml" \
        ./scripts/stage-liteinst-runtime.sh dev \
        "$scratch/producer/libhermit_liteinst_detcore.so" "$scratch/keyed-target"
    printf 'second\n' >"$scratch/source-key/input"
    FAKE_CARGO_LOG="$scratch/target-log" \
        FAKE_EXPECT_CONFIG="$scratch/cargo-config.toml" CARGO="$scratch/fake-cargo" \
        HERMIT_LITEINST_HERMIT_ROOT="$scratch/source-key" \
        HERMIT_LITEINST_REVERIE_ROOT="$scratch/reverie-key" \
        HERMIT_LITEINST_CARGO_CONFIG="$scratch/cargo-config.toml" \
        ./scripts/stage-liteinst-runtime.sh dev \
        "$scratch/producer/libhermit_liteinst_detcore.so" "$scratch/keyed-target"
    printf 'first\n' >"$scratch/source-key/input"
    printf 'second\n' >"$scratch/reverie-key/input"
    FAKE_CARGO_LOG="$scratch/target-log" \
        FAKE_EXPECT_CONFIG="$scratch/cargo-config.toml" CARGO="$scratch/fake-cargo" \
        HERMIT_LITEINST_HERMIT_ROOT="$scratch/source-key" \
        HERMIT_LITEINST_REVERIE_ROOT="$scratch/reverie-key" \
        HERMIT_LITEINST_CARGO_CONFIG="$scratch/cargo-config.toml" \
        ./scripts/stage-liteinst-runtime.sh dev \
        "$scratch/producer/libhermit_liteinst_detcore.so" "$scratch/keyed-target"
    printf 'first\n' >"$scratch/reverie-key/input"
    printf '[net]\noffline = false\n' >"$scratch/cargo-config.toml"
    FAKE_CARGO_LOG="$scratch/target-log" \
        FAKE_EXPECT_CONFIG="$scratch/cargo-config.toml" CARGO="$scratch/fake-cargo" \
        HERMIT_LITEINST_HERMIT_ROOT="$scratch/source-key" \
        HERMIT_LITEINST_REVERIE_ROOT="$scratch/reverie-key" \
        HERMIT_LITEINST_CARGO_CONFIG="$scratch/cargo-config.toml" \
        ./scripts/stage-liteinst-runtime.sh dev \
        "$scratch/producer/libhermit_liteinst_detcore.so" "$scratch/keyed-target"
    printf '[net]\noffline = true\n' >"$scratch/cargo-config.toml"
    FAKE_CARGO_LOG="$scratch/target-log" \
        FAKE_EXPECT_CONFIG="$scratch/cargo-config.toml" CARGO="$scratch/fake-cargo" \
        HERMIT_LITEINST_HERMIT_ROOT="$scratch/source-key" \
        HERMIT_LITEINST_REVERIE_ROOT="$scratch/reverie-key" \
        HERMIT_LITEINST_CARGO_CONFIG="$scratch/cargo-config.toml" \
        HERMIT_LITEINST_RUNTIME_KIND=private-crt \
        ./scripts/stage-liteinst-runtime.sh dev \
        "$scratch/producer/hermit_liteinst_detcore_private.elf" "$scratch/keyed-target"
    mapfile -t keyed_targets <"$scratch/target-log"
    check 'stage cache key distinguishes Hermit source, Reverie source, Cargo configuration, and runtime kind' \
        5/5 \
        "${#keyed_targets[@]}/$(printf '%s\n' "${keyed_targets[@]}" | sort -u | wc -l)"

    mkdir -p "$scratch/source-record-failure/bin"
    cat >"$scratch/source-record-failure/bin/mv" <<'FAKE_MV'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${!#} == "$FAKE_FAIL_DEST" ]]; then
    exit 66
fi
exec /bin/mv "$@"
FAKE_MV
    chmod +x "$scratch/source-record-failure/bin/mv"
    local preserved_record="$scratch/source-record-failure/source-record.json"
    printf 'old source record\n' >"$preserved_record"
    if PATH="$scratch/source-record-failure/bin:$PATH" \
        FAKE_FAIL_DEST="$preserved_record" CARGO="$scratch/fake-cargo" \
        HERMIT_LITEINST_REVERIE_ROOT="$PWD" \
        HERMIT_LITEINST_SOURCE_RECORD="$preserved_record" \
        ./scripts/stage-liteinst-runtime.sh dev \
        "$scratch/source-record-failure/libhermit_liteinst_detcore.so" \
        "$scratch/source-record-failure-target" \
        >"$scratch/source-record-failure.out" \
        2>"$scratch/source-record-failure.err"; then
        printf 'FAIL the standalone producer reported success after source-record publication failed\n' >&2
        failures=$((failures + 1))
    else
        check 'source-record publication failure preserves the old record' \
            0765e2ff90a4c54fc4b6e89732fa761fa84c551fe3edca65a6a704e3d857e220 \
            "$(sha256sum -- "$preserved_record" | cut -d ' ' -f 1)"
    fi

    if ((failures)); then
        printf 'liteinst-strict-node --self-test: %d case(s) failed\n' "$failures" >&2
        return 1
    fi
    printf 'liteinst-strict-node --self-test: all %d cases pass\n' "$checks"
    return 0
}

cd "$(dirname "$0")/.." || exit 1

if [[ ${1:-} == --self-test ]]; then
    self_test
    exit $?
fi

if [[ ${1:-} == -- ]]; then
    shift
fi
if (($# == 0)); then
    usage
    exit 2
fi

verdict="$(classify)"
case "$verdict" in
    missing\ *)
        echo "liteinst-strict: NO RESULT -- no staged LiteInst runtime at ${verdict#missing }." >&2
        echo '  This is a SETUP condition, not a product failure: every test in this' >&2
        echo '  target loads that runtime, so nothing about LiteInst was measured.' >&2
        echo '  Stage the runtime and source record, rebuild Hermit with that record, then build hermit-install.' >&2
        exit 75
        ;;
    incomplete\ *)
        echo "liteinst-strict: NO RESULT -- ${verdict#incomplete } has no complete provenance pair." >&2
        echo '  This is a SETUP condition, not a product failure: the runtime cannot be' >&2
        echo '  validated, so hermit refuses the backend and no test reaches the product.' >&2
        echo '  Restage the runtime and source record, rebuild Hermit with that record, then build hermit-install.' >&2
        exit 75
        ;;
esac

exec "$@"
