#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Single source of truth for Hermit's test matrix.
#
#   validate.sh ─┐
#                ├─> scripts/test-suite.sh  <─ .github/workflows/ci.yml
#                │      (this file)
#
# Both the local validator and CI describe WHAT to run in terms of the named
# "tiers" defined here, so the two can no longer drift. Each tier is a small
# function that runs the exact cargo/script commands for one concern.
#
# Usage:
#   test-suite.sh <tier> [<tier> ...]   Run one or more tiers by name.
#   test-suite.sh --local               Run the local-dev matrix (skips tiers
#                                        whose hardware capability is absent,
#                                        with a notice).
#   test-suite.sh --portable            Run the tiers that need no special
#                                        hardware (the GitHub-hosted CI job).
#   test-suite.sh --hardware            Run the capability-gated tiers (the
#                                        self-hosted / PMU CI job).
#   test-suite.sh --ci                  Run the full suite (portable + hardware)
#                                        and FAIL LOUDLY if a required capability
#                                        is missing.
#   test-suite.sh --quick               Fast smoke check (build + Hermit smoke).
#   test-suite.sh --list [MODE] [--plain]
#                                        List the tiers (optionally for a mode).
#                                        --plain prints "<fg|bg> <tier>" lines
#                                        consumed by validate.sh.
#
# Capability gating follows the repository's "fail loudly instead of skipping"
# policy: --ci treats a missing PMU / mount-namespace support as an error, while
# --local downgrades it to a skip so a developer laptop still gets useful signal.

set -uo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT_DIR
cd "$ROOT_DIR" || exit 1

# Deny warnings for every compiler/rustdoc invocation, preserving caller flags.
export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }-D warnings"
export RUSTDOCFLAGS="${RUSTDOCFLAGS:+${RUSTDOCFLAGS} }-D warnings"

# Mode is exported so gating helpers can distinguish --ci (fail) from --local
# (skip). Individual tier invocations default to "direct": no aggregate gating,
# the caller (e.g. a ci.yml `if:` guard) is responsible.
TS_MODE="${TS_MODE:-direct}"

readonly FLAKY_CRATE="hermetic_infra_hermit_flaky-tests"
readonly HERMIT_BIN="$ROOT_DIR/target/debug/hermit"
readonly HERMIT_SMOKE_TIMEOUT="30s"
readonly SMOKE_MARKER="hermit-validation-smoke"
declare -ar HERMIT_RUN_ARGS=(
    run
    --base-env=minimal
    --no-virtualize-cpuid
    --preemption-timeout=disabled
)

# Use the "ci" nextest profile (JUnit output, slow-test reporting) whenever we
# are running under CI or an aggregate CI mode; otherwise the default profile.
function ts_nextest {
    local -a cmd=(cargo nextest run)
    if [[ -n ${NEXTEST_PROFILE:-} ]]; then
        cmd+=(--profile "$NEXTEST_PROFILE")
    elif [[ -n ${CI:-} || $TS_MODE == ci || $TS_MODE == portable || $TS_MODE == hardware ]]; then
        cmd+=(--profile ci)
    fi
    "${cmd[@]}" "$@"
}

# ---------------------------------------------------------------------------
# Capability detection. Each returns 0 when the capability is present.
# ---------------------------------------------------------------------------

function ts_cap_pmu {
    [[ ${HERMIT_HAS_PMU:-} == 1 ]] && return 0
    [[ ${HERMIT_HAS_PMU:-} == 0 ]] && return 1
    [[ -d /sys/bus/event_source/devices/cpu ]] && return 0
    [[ -d /sys/bus/event_source/devices/cpu_core ]] && return 0
    return 1
}

function ts_cap_namespaces {
    [[ ${HERMIT_HAS_NAMESPACES:-} == 1 ]] && return 0
    [[ ${HERMIT_HAS_NAMESPACES:-} == 0 ]] && return 1
    unshare --user --map-root-user --pid --fork --uts --net --mount sh -c \
        'mount -t proc proc /proc && mount --bind /tmp /tmp && mount --make-rshared /tmp && umount /tmp' \
        >/dev/null 2>&1
}

function ts_cap_rr {
    [[ -f "$ROOT_DIR/third-party/rr/src/test/util.h" ]]
}

function ts_cap_kvm {
    [[ -r /dev/kvm && -w /dev/kvm ]]
}

function ts_cap_dbi {
    [[ -n ${DYNAMORIO_HOME:-} && -n ${HERMIT_DRRUN:-} && -n ${HERMIT_DBI_CLIENT:-} ]]
}

# Whether the capability a tier declares (column 6) is available. An empty
# requirement ("-") is always present; unknown requirements are assumed present
# (they are gated inside the tier itself, e.g. kvm/dbi in backend-parity).
function ts_cap_present {
    case "$1" in
        -) return 0 ;;
        pmu) ts_cap_pmu ;;
        namespaces) ts_cap_namespaces ;;
        rr) ts_cap_rr ;;
        *) return 0 ;;
    esac
}

# ---------------------------------------------------------------------------
# Tier registry.
#
# Columns (tab-separated):
#   tier | schedule(fg|bg) | portable | local | hardware | capability | description
#
# capability is one of: -, pmu, namespaces, rr  (the aggregate gate checks
# these; other capabilities such as kvm/dbi are handled inside their tier).
# ---------------------------------------------------------------------------
function ts_registry {
    cat <<'EOF'
build	fg	1	1	0	-	Build the workspace
clippy	bg	1	1	0	-	Clippy lints (deny warnings)
fmt	bg	1	1	0	-	Rustfmt check
doc	bg	1	1	0	-	Build rustdoc
doctest	bg	1	1	0	-	Run documentation tests
unit-regular	fg	1	0	0	-	Nextest: workspace unit/integration (no detcore/hermit)
unit-hermit-bins	fg	1	0	0	-	cargo test -p hermit --lib --bins
unit-detcore-bins	fg	1	0	0	-	cargo test -p detcore --lib --bins (+ getrandom)
unit-full	fg	0	1	0	namespaces	Nextest: full workspace minus detcore (local superset)
smoke	fg	0	1	0	-	Hermit run / determinism / verify smoke tests
detcore-cpuid	fg	0	1	1	pmu	Detcore CPUID + RDRAND/RDSEED masking
detcore-pmu	fg	0	1	1	pmu	Detcore PMU timing tests (tests_time, getrandom)
detcore-parallel	fg	0	1	1	pmu	Detcore PMU parallel futex/memory tests
hermit-determinism	fg	0	0	1	namespaces	Hermit stable integration + determinism suite
hermit-matrices	fg	0	1	1	namespaces	Hermit record/replay + strict-mode matrices
leveldb	fg	0	1	1	namespaces	LevelDB focused strict-determinism test
ratchet	fg	0	1	1	namespaces	Fail-closed unsupported-syscall ratchet
debugger	fg	0	1	1	namespaces	gdb/lldb debugger integration tests
rr-suite	fg	0	1	1	rr	rr syscall edge-case suite (third-party/rr)
backend-parity	fg	0	1	1	-	Backend parity ratchet (ptrace; kvm/dbi if configured)
analyze	fg	0	1	0	pmu	hermit analyze scenarios + schedule-search E2E
EOF
}

function ts_tier_field {
    # $1 = tier, $2 = 1-based column index
    ts_registry | awk -F'\t' -v t="$1" -v c="$2" '$1==t{print $c; found=1} END{if(!found) exit 1}'
}

function ts_tiers_for_mode {
    # $1 = local|portable|hardware
    local col
    case "$1" in
        portable) col=3 ;;
        local) col=4 ;;
        hardware) col=5 ;;
        *) return 1 ;;
    esac
    ts_registry | awk -F'\t' -v c="$col" '$c==1{print $1}'
}

# ---------------------------------------------------------------------------
# Tier implementations.
# ---------------------------------------------------------------------------

function tier_build { cargo build --workspace; }
function tier_clippy { cargo clippy --workspace --all-targets -- -D warnings; }
function tier_fmt { cargo fmt --all -- --check; }
function tier_doc { cargo doc --workspace --no-deps; }
function tier_doctest { cargo test --workspace --doc; }

function tier_unit-regular {
    ts_nextest --workspace --exclude detcore --exclude hermit --exclude "$FLAKY_CRATE"
}

function tier_unit-hermit-bins { cargo test -p hermit --lib --bins; }

function tier_unit-detcore-bins {
    cargo test -p detcore --lib --bins
    cargo test -p detcore --test tests_misc getrandom_intercepted -- --exact
}

function tier_unit-full {
    ts_nextest --workspace --exclude detcore --exclude "$FLAKY_CRATE"
}

function tier_smoke {
    local first second
    first=$(timeout "$HERMIT_SMOKE_TIMEOUT" "$HERMIT_BIN" "${HERMIT_RUN_ARGS[@]}" -- /bin/echo "$SMOKE_MARKER") || return $?
    if [[ $first != "$SMOKE_MARKER" ]]; then
        printf 'Unexpected Hermit stdout: %q\n' "$first" >&2
        return 1
    fi
    second=$(timeout "$HERMIT_SMOKE_TIMEOUT" "$HERMIT_BIN" "${HERMIT_RUN_ARGS[@]}" -- /bin/echo "$SMOKE_MARKER") || return $?
    if [[ $first != "$second" ]]; then
        echo "Hermit stdout differed between identical runs:" >&2
        diff -u <(printf '%s\n' "$first") <(printf '%s\n' "$second") >&2 || true
        return 1
    fi
    timeout "$HERMIT_SMOKE_TIMEOUT" "$HERMIT_BIN" "${HERMIT_RUN_ARGS[@]}" --verify -- /bin/echo "$SMOKE_MARKER"
}

function tier_detcore-cpuid {
    cargo test -p detcore --test tests_misc has_rdrand_without_detcore -- --exact
    cargo test -p detcore --test tests_misc rdrand_rdseed_is_masked -- --exact
}

function tier_detcore-pmu {
    cargo test -p detcore --test tests_misc getrandom_intercepted -- --exact
    cargo test -p detcore --test tests_time -- --test-threads=4
}

function tier_detcore-parallel {
    cargo test -p detcore --test tests_parallelism futex_wait_parent -- --test-threads=3
    cargo test -p detcore --test tests_parallelism 'mem_race::' -- --test-threads=4
    cargo test -p detcore --test tests_parallelism 'mem_print_race::' -- --test-threads=4
}

function tier_hermit-determinism {
    cargo test -p hermit --lib --bins
    local test
    for test in \
        arbitrary_binaries \
        cli \
        clock_determinism \
        epoll_determinism \
        mmap_determinism \
        procfs_determinism \
        signal_determinism
    do
        cargo test -p hermit --test "$test" -- --test-threads=1
    done
}

function tier_hermit-matrices {
    cargo test -p hermit --test record_replay record_replay_matrix -- --exact --test-threads=1
    cargo test -p hermit --test hermit_modes strict_mode_matrix -- --exact --test-threads=1
}

function tier_leveldb {
    local build_dir source_dir tag
    tag="${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-$$}"
    source_dir="$ROOT_DIR/target/hermit-leveldb-$tag"
    build_dir="$ROOT_DIR/target/hermit-leveldb-build-$tag"
    ./hermit-cli/tests/prepare_leveldb.sh "$source_dir" "$build_dir"
    HERMIT_LEVELDB_BUILD_DIR="$build_dir" \
        cargo test -p hermit --release --test leveldb \
        focused_leveldb_tests_are_deterministic_under_strict -- --exact --test-threads=1
}

function tier_ratchet { ./scripts/test-fail-closed.sh; }

function tier_debugger { ./tests/debugger/run_debugger_tests.sh; }

function tier_rr-suite { cargo test -p hermit --test rr_suite -- --ignored; }

function tier_backend-parity {
    local runner=experiments/backend-parity_20260722/run_matrix.py
    python3 "$runner" --backend ptrace
    if "$HERMIT_BIN" run --help 2>/dev/null | grep -q -- '--backend'; then
        if ts_cap_kvm; then
            python3 "$runner" --backend kvm --require-backend
        else
            echo "::notice::KVM parity blocked: /dev/kvm is not readable and writable"
        fi
        if ts_cap_dbi; then
            python3 "$runner" --backend dbi --require-backend
        else
            echo "::notice::DBI parity blocked: DynamoRIO/client environment is not configured"
        fi
    else
        echo "::notice::DBI/KVM parity blocked: backend selector is not integrated yet"
    fi
}

function tier_analyze {
    cargo test -p hermit --test analyze -- --ignored
    ./tests/util/hermit_analyze_e2e.sh
}

# ---------------------------------------------------------------------------
# Dispatch.
# ---------------------------------------------------------------------------

# Run a single tier by name. Returns the tier's exit status.
function ts_run_tier {
    local tier=$1
    if ! declare -F "tier_$tier" >/dev/null; then
        echo "test-suite.sh: unknown tier '$tier'" >&2
        return 2
    fi
    printf '::group::test-suite tier: %s\n' "$tier"
    local status=0
    "tier_$tier" || status=$?
    printf '::endgroup::\n'
    return "$status"
}

# Decide whether a capability-gated tier can run in the current aggregate mode.
# Returns: 0 run, 1 skip (local), 2 fail (ci).
function ts_gate {
    local cap=$1 desc=$2
    case "$cap" in
        -) return 0 ;;
        pmu) ts_cap_pmu && return 0 ;;
        namespaces) ts_cap_namespaces && return 0 ;;
        rr) ts_cap_rr && return 0 ;;
        *) return 0 ;;
    esac
    if [[ $TS_MODE == ci || $TS_MODE == hardware ]]; then
        echo "::error::required capability '$cap' missing for tier '$desc'" >&2
        return 2
    fi
    echo "SKIP ($TS_MODE): tier '$desc' requires '$cap', which is unavailable" >&2
    return 1
}

# Run a list of tiers, honoring capability gating, and return nonzero if any
# tier failed (skips are not failures).
function ts_run_tiers {
    local -a tiers=("$@")
    local tier cap gate_status failed=0 ran=0 skipped=0
    for tier in "${tiers[@]}"; do
        cap=$(ts_tier_field "$tier" 6)
        ts_gate "$cap" "$tier"
        gate_status=$?
        if ((gate_status != 0)); then
            if ((gate_status == 1)); then
                skipped=$((skipped + 1))
            else
                failed=$((failed + 1))
            fi
            continue
        fi
        ran=$((ran + 1))
        if ! ts_run_tier "$tier"; then
            failed=$((failed + 1))
            [[ ${TS_FAIL_FAST:-0} == 1 ]] && break
        fi
    done
    printf 'test-suite: %d ran, %d skipped, %d failed\n' "$ran" "$skipped" "$failed" >&2
    ((failed == 0))
}

function ts_list {
    local mode="${1:-all}" plain="${2:-}"
    local -a tiers
    if [[ $mode == all ]]; then
        mapfile -t tiers < <(ts_registry | cut -f1)
    else
        mapfile -t tiers < <(ts_tiers_for_mode "$mode")
    fi
    local tier sched desc cap
    for tier in "${tiers[@]}"; do
        sched=$(ts_tier_field "$tier" 2)
        if [[ $plain == --plain ]]; then
            # Plain output drives an executor (validate.sh), so only advertise
            # tiers whose hardware capability is actually present; note the rest.
            cap=$(ts_tier_field "$tier" 6)
            if ts_cap_present "$cap"; then
                printf '%s\t%s\n' "$sched" "$tier"
            else
                printf "SKIP: tier '%s' requires '%s' (unavailable on this host)\n" \
                    "$tier" "$cap" >&2
            fi
        else
            desc=$(ts_tier_field "$tier" 7)
            printf '%-20s [%s] %s\n' "$tier" "$sched" "$desc"
        fi
    done
}

function main {
    if [[ $# -eq 0 ]]; then
        echo "test-suite.sh: no tier or mode given (see --list)" >&2
        return 2
    fi

    case "$1" in
        --list)
            shift
            local mode=all plain=
            for arg in "$@"; do
                case "$arg" in
                    --plain) plain=--plain ;;
                    *) mode=$arg ;;
                esac
            done
            ts_list "$mode" "$plain"
            return 0
            ;;
        --local)
            TS_MODE=local
            mapfile -t _tiers < <(ts_tiers_for_mode local)
            ts_run_tiers "${_tiers[@]}"
            ;;
        --portable)
            TS_MODE=portable
            mapfile -t _tiers < <(ts_tiers_for_mode portable)
            ts_run_tiers "${_tiers[@]}"
            ;;
        --hardware)
            TS_MODE=hardware
            mapfile -t _tiers < <(ts_tiers_for_mode hardware)
            ts_run_tiers "${_tiers[@]}"
            ;;
        --ci)
            TS_MODE=ci
            mapfile -t _tiers < <(ts_tiers_for_mode portable; ts_tiers_for_mode hardware)
            ts_run_tiers "${_tiers[@]}"
            ;;
        --quick)
            # Fast smoke check for inner-loop iteration: build just enough to run
            # the Hermit smoke/determinism/verify probes.
            TS_MODE=local
            ts_run_tiers build smoke
            ;;
        --*)
            echo "test-suite.sh: unknown option '$1'" >&2
            return 2
            ;;
        *)
            # One or more explicit tier names.
            local status=0
            for tier in "$@"; do
                ts_run_tier "$tier" || status=$?
            done
            return "$status"
            ;;
    esac
}

# Only dispatch when executed directly; sourcing exposes the tier/helper
# functions (used by validate.sh and by unit checks) without running anything.
if [[ ${BASH_SOURCE[0]} == "${0}" ]]; then
    main "$@"
fi
