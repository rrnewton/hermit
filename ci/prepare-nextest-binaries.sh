#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

case ${1:-} in
    portable | privileged)
        lane=$1
        ;;
    *)
        printf 'usage: %s <portable|privileged>\n' "$0" >&2
        exit 2
        ;;
esac

parent="$ROOT_DIR/target/ci"
published="$parent/nextest-binaries"
mkdir -p "$parent"
scratch=$(mktemp -d "$parent/.nextest-binaries.XXXXXXXX")
cleanup() {
    rm -rf -- "$scratch"
}
trap cleanup EXIT

cargo metadata --format-version 1 --locked >"$scratch/cargo-metadata.json"

record() {
    local name=$1
    shift
    local -a command=() selection=()
    while (($#)) && [[ $1 != -- ]]; do
        command+=("$1")
        shift
    done
    if (($# == 0)); then
        printf 'prepare-nextest-binaries: %s has no command/selection separator\n' "$name" >&2
        return 2
    fi
    shift
    selection=("$@")
    ./ci/run-nextest-counted.sh --print-build-selection "${selection[@]}" \
        >"$scratch/$name.selection"
    "${command[@]}" "${selection[@]}" \
        --message-format json --list-type binaries-only >"$scratch/$name.json"
}

if [[ $lane == portable ]]; then
    record regular ./ci/run-with-reverie-dbt-budget.sh cargo nextest list -- \
        ${CI:+--profile ci} \
        --workspace \
        --exclude hermit-detcore \
        --exclude hermit \
        --exclude hermetic_infra_hermit_flaky-tests

    record hermit-unit ./ci/run-with-reverie-dbt-budget.sh cargo nextest list -- \
        ${CI:+--profile ci} -p hermit --features third-party-backends --lib --bins
    record hermit-integration ./ci/run-with-reverie-dbt-budget.sh cargo nextest list -- \
        ${CI:+--profile ci} -p hermit --features third-party-backends \
        --test aio_nr_determinism --test arch_status_determinism \
        --test chaos_sched_yield_progress --test chaos_stress_pmu_detection \
        --test chown_virtual_root_identity --test clock_determinism \
        --test clock_discipline_determinism --test container_init_deadline \
        --test cpufreq_avg_determinism --test epoll_determinism \
        --test epoll_pwait_zero_timeout_progress --test file_nr_determinism \
        --test fp_reduction_determinism --test futex2_refusal \
        --test hashseed_determinism --test inode_nr_determinism \
        --test kernel_keyring --test key_users_determinism \
        --test mmap_determinism --test node_vmstat_determinism \
        --test numa_maps_determinism --test perf_event_refusal \
        --test pidfd_creation --test process_isolation_refusals \
        --test proc_fdinfo_determinism --test proc_locks_determinism \
        --test procfs_determinism --test procfs_positioned_determinism \
        --test pty_nr_determinism --test python_stdlib \
        --test self_sched_determinism --test self_schedstat_determinism \
        --test signal_determinism --test smaps_determinism \
        --test smaps_rollup_determinism --test softnet_stat_determinism \
        --test sockstat_determinism --test swaps_determinism \
        --test thp_stats_determinism --test zero_copy_pipe_fallback
    record arbitrary-binaries ./ci/run-with-reverie-dbt-budget.sh cargo nextest list -- \
        ${CI:+--profile ci} -p hermit --features third-party-backends \
        --test arbitrary_binaries
    record liteinst-advanced ./ci/run-with-reverie-dbt-budget.sh cargo nextest list -- \
        ${CI:+--profile ci} -p hermit --features third-party-backends \
        --test liteinst_advanced
    record sabre-examples ./ci/run-with-reverie-dbt-budget.sh cargo nextest list -- \
        ${CI:+--profile ci} -p hermit --features third-party-backends \
        --test sabre_examples
    record app-strict-verify ./ci/run-with-reverie-dbt-budget.sh cargo nextest list -- \
        ${CI:+--profile ci} -p hermit --features third-party-backends \
        --test app_strict_verify
    record command-strict-verify ./ci/run-with-reverie-dbt-budget.sh cargo nextest list -- \
        ${CI:+--profile ci} -p hermit --features third-party-backends \
        --test command_strict_verify
    record ignored-syscall-regressions ./ci/run-with-reverie-dbt-budget.sh cargo nextest list -- \
        ${CI:+--profile ci} -p hermit --features third-party-backends \
        --test epoll_determinism --test rcx_canonicalization
    record rr-suite ./ci/run-with-reverie-dbt-budget.sh cargo nextest list -- \
        ${CI:+--profile ci} -p hermit --features third-party-backends \
        --test rr_suite

    record detcore-unit cargo nextest list -- \
        ${CI:+--profile ci} -p hermit-detcore --lib --bins
    record detcore-parallel cargo nextest list -- \
        ${CI:+--profile ci} -p hermit-detcore --test tests_parallelism
fi

record detcore-misc cargo nextest list -- \
    ${CI:+--profile ci} -p hermit-detcore --test tests_misc
record cli ./ci/run-with-reverie-dbt-budget.sh cargo nextest list -- \
    ${CI:+--profile ci} -p hermit --features third-party-backends --test cli
record hermit-modes ./ci/run-with-reverie-dbt-budget.sh cargo nextest list -- \
    ${CI:+--profile ci} -p hermit --features third-party-backends --test hermit_modes

# hermit_modes executes these binaries as runtime inputs. Build them here so
# the timed test process never conditionally invokes Cargo based on shared
# target-directory state.
./ci/run-with-reverie-dbt-budget.sh cargo build --locked \
    -p hermetic_infra_hermit_tests --bins

for metadata in "$scratch"/*.json; do
    if [[ $metadata == */cargo-metadata.json ]]; then
        jq -e '.workspace_root and .target_directory and (.packages | length > 0)' \
            "$metadata" >/dev/null
    else
        jq -e '."rust-build-meta" and (."rust-binaries" | type == "object" and length > 0)' \
            "$metadata" >/dev/null
    fi
done

# No Cargo command runs after this point. Exercise every recorded path against
# the final shared-target state so a later selection cannot silently leave an
# earlier metadata file pointing at a removed executable.
for metadata in "$scratch"/*.json; do
    [[ $metadata == */cargo-metadata.json ]] && continue
    cargo nextest list \
        --cargo-metadata "$scratch/cargo-metadata.json" \
        --binaries-metadata "$metadata" \
        --target-dir-remap "$ROOT_DIR/target" \
        --message-format json \
        >/dev/null
done

if [[ -L $published || (-e $published && ! -d $published) ]]; then
    printf 'prepare-nextest-binaries: refusing to replace non-directory output: %s\n' \
        "$published" >&2
    exit 2
fi
rm -rf -- "$published"
mv -- "$scratch" "$published"
trap - EXIT

printf 'prepared nextest binaries metadata for %s at %s\n' "$lane" "$published"
