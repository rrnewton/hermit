#!/usr/bin/env bash

set -uo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly SCRIPT_DIR
readonly RESULT_WRITER="$SCRIPT_DIR/nextest-test-results.rs"
readonly TIMEOUT_CONFIG_WRITER="$SCRIPT_DIR/nextest-timeout-config.rs"
readonly PREBUILT_RUST_SCRIPT="$SCRIPT_DIR/rust-script-bin/rust-script"

nextest_config=
cpu_measurement_dir=
calibration_launch=
calibration_host=false
nextest_budget_map=
nextest_budget_sha=
retain_evidence=
retention_status=0
retention_io_failed=0
retention_capture_closed=0
retention_stage=admission
retention_config_serial=0
retention_list_status=not-run
retention_run_status=not-run
retention_parser_status=not-run
retention_map_status=not-run
retention_config_status=not-run
retention_budget_status=not-run
retention_budget_context_status=not-run
retention_cpu_wrapper_status=not-run
retention_original_argv=("$@")

function retention_note_failure {
    retention_status=2
    retention_io_failed=1
    printf 'run-nextest-counted: evidence capture incomplete: %s; fix the I/O problem and retry with a NEW destination\n' "$*" >&2
    if [[ -n $retain_evidence && -d $retain_evidence && ! -L $retain_evidence ]]; then
        printf '%s\n' "$*" >>"$retain_evidence/capture-errors.txt" || :
    fi
}

# Destination names below are internal constants. Never follow evidence-source
# symlinks, and never overwrite a file from this or an earlier invocation.
function retention_copy {
    local source=$1 name=$2
    [[ -n $retain_evidence ]] || return 0
    if [[ -L $source || ( -e $source && ! -f $source ) ]]; then
        retention_note_failure "non-regular source for $name: $source"
        return 2
    fi
    if [[ ! -e $source ]]; then
        (set -o noclobber; printf '%s\n' "$source" >"$retain_evidence/$name.absent") || {
            retention_note_failure "cannot record absence of $name"; return 2;
        }
        return 0
    fi
    (set -o noclobber; cat -- "$source" >"$retain_evidence/$name") || {
        retention_note_failure "cannot copy $name"; return 2;
    }
    # Require the retained bytes to match the still-existing source before cleanup.
    cmp -s -- "$source" "$retain_evidence/$name" || {
        retention_note_failure "source changed or copy incomplete for $name"; return 2;
    }
}

function retention_capture_config {
    [[ -n $retain_evidence && -n $nextest_config && $retention_capture_closed == 0 ]] || return 0
    [[ -f $nextest_config ]] || retention_note_failure 'generated config missing before cleanup'
    retention_config_serial=$((retention_config_serial + 1))
    retention_copy "$nextest_config" "config-$retention_config_serial.toml" || :
}

# Compare publication locations, not executable identities. Atomic result
# writers rename a sibling temporary over the final entry, so canonicalize the
# parent and retain the final name (do not dereference a final symlink). CPU
# sidecars use the same caller-relative spelling. No publication path is changed.
function retention_check_publication_overlap {
    local destination=$1 count_path=${DAGRUN_TEST_COUNTS_PATH:--} resolved_cpu
    local publication parent leaf canonical_parent location
    local -a publications=()
    resolved_cpu=$(configured_cpu_report_path) || return $?
    if [[ $count_path != - ]]; then
        publications+=("$count_path" "$count_path.publication-recovery")
    fi
    if [[ $resolved_cpu != - ]]; then
        publications+=("$resolved_cpu" "$resolved_cpu.launch.json" "$resolved_cpu.budgets.json")
    fi
    for publication in "${publications[@]}"; do
        # Keep shell metacharacters and newlines literal. The dot sentinel keeps
        # command substitution from stripping a newline in a directory name.
        case $publication in
            */*) parent=${publication%/*}; leaf=${publication##*/}; [[ -n $parent ]] || parent=/ ;;
            *) parent=.; leaf=$publication ;;
        esac
        canonical_parent=$(realpath -m -- "$parent" && printf '.') || {
            printf 'run-nextest-counted: cannot resolve publication parent %q; choose a usable output path outside the capture directory; see --help\n' "$parent" >&2
            return 2
        }
        canonical_parent=${canonical_parent%$'\n.'}
        location=${canonical_parent%/}/$leaf
        if [[ $canonical_parent == "$destination" || $canonical_parent == "$destination/"* ||
            $location == "$destination" || $location == "$destination/"* ||
            $destination == "${location%/}/"* ]]; then
            printf 'run-nextest-counted: output publication %q overlaps capture directory %q; choose a NEW capture directory separate from DAGRUN_TEST_COUNTS_PATH, its recovery sibling, HERMIT_NEXTEST_CPU_REPORT_PATH (or its default), and CPU sidecars; see --help\n' "$publication" "$destination" >&2
            return 2
        fi
    done
}

function retention_begin {
    local destination=$1
    shift
    # mkdir is the exclusive claim, including refusal of dangling symlinks.
    # Require canonical parent and final name; never manufacture parent paths.
    if [[ $destination != /* || $destination == */ || $destination == *$'\n'* ]] ||
        [[ $(realpath -m -- "$destination") != "$destination" ]]; then
        printf 'run-nextest-counted: invalid capture destination %q; use a canonical absolute path without trailing slash or newline, under an existing parent; see --help\n' "$destination" >&2
        return 2
    fi
    # Refuse before claiming the directory, installing EXIT capture, allocating
    # temporary files, or invoking any helper/runner. Caller publications survive.
    retention_check_publication_overlap "$destination" || return $?
    if ! (umask 077; mkdir -- "$destination"); then
        printf 'run-nextest-counted: cannot exclusively create capture destination %q; choose a NEW nonexistent directory under an existing writable parent; existing files/directories/symlinks are never reused\n' "$destination" >&2
        return 2
    fi
    retain_evidence=$destination
    retention_status=0
    retention_io_failed=0
    retention_capture_closed=0
    retention_config_serial=0
    retention_list_status=not-run
    retention_run_status=not-run
    retention_parser_status=not-run
    retention_map_status=not-run
    retention_config_status=not-run
    retention_budget_status=not-run
    retention_budget_context_status=not-run
    retention_cpu_wrapper_status=not-run
    retention_stage=setup
    # Install before the first temporary allocation, so early failures retain
    # absence/status and still use the existing ordinary cleanup functions.
    trap 'retention_exit "$?"' EXIT
    (set -o noclobber; { if ((${#retention_original_argv[@]})); then printf '%s\0' "${retention_original_argv[@]}"; fi; } >"$retain_evidence/invocation.argv0") || return 2
    (set -o noclobber; { if (($#)); then printf '%s\0' "$@"; fi; } >"$retain_evidence/nextest-selection.argv0") || return 2
    (set -o noclobber; printf '%s\n' "$PWD" >"$retain_evidence/cwd.txt") || return 2
    # This allowlist is selection/configuration evidence, not a credential dump.
    local key
    for key in NEXTEST_PREPARED_BUILD_SELECTION HERMIT_PREPARED_NEXTEST_REQUIRED \
        HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED HERMIT_RUST_SCRIPT_ARTIFACT_ROOT \
        NEXTEST_EXPECTED_EXECUTED NEXTEST_PROFILE NEXTEST_TEST_THREADS \
        HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER \
        DAGRUN_TEST_COUNTS_PATH HERMIT_NEXTEST_CPU_REPORT_PATH \
        CARGO_TARGET_DIR CARGO_BUILD_TARGET RUSTFLAGS CARGO_ENCODED_RUSTFLAGS; do
        if [[ -v $key ]]; then
            printf '%s\0%s\0' "$key" "${!key}"
        fi
    done >"$retain_evidence/selection-environment.argv0" || return 2
    retention_copy "$SCRIPT_DIR/run-nextest-counted.sh" wrapper.sh || return 2
    retention_copy "$RESULT_WRITER" parser.rs || return 2
    retention_copy "$TIMEOUT_CONFIG_WRITER" timeout-config.rs || return 2
    retention_copy "$SCRIPT_DIR/nextest-binaries.rs" nextest-binaries.rs || return 2
    retention_copy "$SCRIPT_DIR/../.config/nextest.toml" original-nextest.toml || return 2
    retention_copy "$SCRIPT_DIR/../.config/nextest-budgets.json" original-budgets.json || return 2
    retention_copy "$SCRIPT_DIR/../target/ci/nextest-binaries/current.json" prepared-record.before.observed.json || return 2
    # Ordinary observed publications may predate this invocation. Preserve the
    # baseline and never relabel a stale file as parser success.
    local published=${DAGRUN_TEST_COUNTS_PATH:--}
    [[ $published == - ]] && published=
    retention_copy "$published" typed-result.before.observed.json || return 2
    # Capture does not observe or attest the actual producer executable. Existing
    # launch/admission policy is unchanged; no PATH/version probe supplies proof.
    printf '%s\n' '{"state":"unattested","reason":"capture saves bytes only; actual list/run producer identity is unavailable","list":null,"run":null}' \
        >"$retain_evidence/producer-identity.json" || return 2
}

function retention_finish {
    local original=$1 source name
    [[ -n $retain_evidence ]] || return "$original"
    retention_stage=${retention_stage:-unknown}
    if [[ -n ${events_log:-} && ! -f $events_log ]]; then
        retention_note_failure 'allocated raw event file is missing'
    fi
    if [[ $retention_list_status != not-run && ! -f ${cpu_inventory:-} ]]; then
        retention_note_failure 'attempted inventory output is missing'
    fi
    if [[ $retention_map_status == 0 && ! -f ${cpu_binary_map:-} ]]; then
        retention_note_failure 'successful binary map output is missing'
    fi
    retention_copy "${events_log:-}" events.jsonl || :
    retention_copy "${cpu_inventory:-}" inventory.json || :
    retention_copy "${cpu_binary_map:-}" binary-map.json || :
    retention_copy "${skid_retry_record:-}" skid-retries.log || :
    source=; [[ -z ${cpu_measurement_dir:-} ]] || source="$cpu_measurement_dir/budget-context.json"
    retention_copy "$source" budget-context.json || :
    source=; [[ -z ${cpu_measurement_dir:-} ]] || source="$cpu_measurement_dir/resolved-budgets.json"
    retention_copy "$source" resolved-budgets.json || :
    if [[ -n $calibration_launch ]]; then
        retention_copy "$calibration_launch" launch.json || :
    else
        retention_copy '' launch.json || :
    fi
    retention_copy "$SCRIPT_DIR/../target/ci/nextest-binaries/current.json" prepared-record.after.observed.json || :
    retention_copy "$SCRIPT_DIR/run-nextest-counted.sh" wrapper.after.sh || :
    retention_copy "$RESULT_WRITER" parser.after.rs || :
    local count_path=${DAGRUN_TEST_COUNTS_PATH:--}
    [[ $count_path == - ]] && count_path=
    retention_copy "$count_path" typed-result.observed.json || :
    [[ ${cpu_report:-} == - ]] && source= || source=${cpu_report:-}
    retention_copy "$source" cpu-report.observed.json || :
    if [[ -n $source ]]; then
        retention_copy "$source.launch.json" launch-publication.observed.json || :
        retention_copy "$source.budgets.json" budget-publication.observed.json || :
    fi
    # Names deliberately say observed: a pre-existing publication is never
    # credited as this invocation's result on a parser/publication refusal.
    if [[ -n ${cpu_attempt_records:-} && -d $cpu_attempt_records && ! -L $cpu_attempt_records ]]; then
        mkdir -- "$retain_evidence/attempts" || retention_note_failure 'cannot create retained attempts'
        # Keep and compare the complete listing, including dotfiles and raw
        # incomplete records. Enumeration errors must not look like zero attempts.
        if find "$cpu_attempt_records" -mindepth 1 -maxdepth 1 -print0 \
            | LC_ALL=C sort -z >"$retain_evidence/attempt-paths.before.argv0"; then
            while IFS= read -r -d '' source; do
                if [[ ! -e $source && ! -L $source ]]; then
                    retention_note_failure "attempt disappeared before capture: $source"
                    continue
                fi
                name=${source##*/}
                retention_copy "$source" "attempts/$name" || :
            done <"$retain_evidence/attempt-paths.before.argv0"
            if ! find "$cpu_attempt_records" -mindepth 1 -maxdepth 1 -print0 \
                | LC_ALL=C sort -z >"$retain_evidence/attempt-paths.after.argv0"; then
                retention_note_failure 'cannot re-enumerate attempts'
            elif ! cmp -s -- "$retain_evidence/attempt-paths.before.argv0" "$retain_evidence/attempt-paths.after.argv0"; then
                retention_note_failure 'attempt population changed during capture'
            fi
        else
            retention_note_failure 'cannot enumerate attempts'
        fi
    elif [[ -n ${cpu_attempt_records:-} && ( -e $cpu_attempt_records || -L $cpu_attempt_records ) ]]; then
        retention_note_failure 'attempt directory is not a normal directory'
    else
        if [[ $retention_run_status != not-run ]]; then
            retention_note_failure 'attempt directory missing after runner invocation'
        fi
        retention_copy '' attempts || :
    fi
    # Retention is only byte capture. The original wrapper has already selected
    # runner/parser precedence; preserve that status unless it was successful.
    if ((retention_io_failed != 0)); then
        retention_incomplete "$original"
        return $?
    fi
    if ! retention_write_status "$original"; then
        retention_note_failure 'cannot write status.json'
        retention_incomplete "$original"
        return $?
    fi
    # Complete bytes, status, and absence observations are checksummed before
    # publishing the completion marker. A late error must not return zero.
    if ! (cd -- "$retain_evidence" && find . -type f ! -path ./SHA256SUMS \
        ! -path ./CAPTURED ! -path ./.CAPTURED.pending -print0 \
        | LC_ALL=C sort -z | xargs -0 -r sha256sum -- >SHA256SUMS &&
        sha256sum --check SHA256SUMS >/dev/null); then
        retention_note_failure 'cannot hash or verify captured evidence'
        retention_incomplete "$original"
        return $?
    fi
    if ! (set -o noclobber; printf 'Bytes captured; verify SHA256SUMS. This does not qualify tests or attest the producer.\n' \
        >"$retain_evidence/.CAPTURED.pending") ||
        ! mv -T -- "$retain_evidence/.CAPTURED.pending" "$retain_evidence/CAPTURED"; then
        retention_note_failure 'cannot publish capture completion marker'
        retention_incomplete "$original"
        return $?
    fi
    return "$original"
}

function retention_write_status {
    local original=$1 final=$1
    ((original != 0 || retention_io_failed == 0)) || final=2
    jq -n --arg stage "$retention_stage" --arg list "$retention_list_status" \
        --arg run "$retention_run_status" --arg parser "$retention_parser_status" \
        --arg map "$retention_map_status" --arg config "$retention_config_status" \
        --arg budget "$retention_budget_status" --arg budget_context "$retention_budget_context_status" \
        --arg cpu_wrapper "$retention_cpu_wrapper_status" --argjson runner "$original" \
        --argjson capture "$retention_status" --argjson io_failed "$retention_io_failed" --argjson final "$final" \
        '{schema:1,stage:$stage,list_status:$list,nextest_status:$run,parser_status:$parser,
          map_status:$map,config_status:$config,budget_status:$budget,budget_context_status:$budget_context,cpu_wrapper_status:$cpu_wrapper,
          runner_status:$runner,capture_status:$capture,capture_io_failed:$io_failed,final_status:$final,
          producer_identity:"unattested",capture_means:"bytes saved only",
          completion_requires_marker_and_valid_checksums:true,observed_publications_are_not_validation:true}' \
        >"$retain_evidence/status.json"
}

function retention_incomplete {
    local original=$1
    # Best effort diagnostic only: I/O failure can prevent even status.json from
    # being written. No completion marker is created on any error path.
    rm -f -- "$retain_evidence/.CAPTURED.pending" || :
    retention_write_status "$original" || :
    ((original != 0)) && return "$original"
    return 2
}

function retention_exit {
    local original=$1 final=$1
    trap - EXIT
    # EXIT can be reached with errexit set by run_nextest; retention errors must
    # never skip ordinary cleanup or replace a failing Nextest exit status.
    set +e
    retention_capture_config
    retention_finish "$original"
    final=$?
    retention_capture_closed=1
    cleanup_nextest_config
    cleanup_cpu_measurement_dir
    [[ -z ${events_log:-} ]] || rm -f -- "$events_log"
    exit "$final"
}


# Do not rely on a nested `/usr/bin/env rust-script` shebang resolving the
# repository wrapper after validation has entered its hosted namespace.  The
# top-level driver was already resolved before that boundary, but these helper
# invocations happen inside it; run 35335623037 resolved the installed compiler
# instead, tried to rebuild nextest-binaries.rs without network access, and all
# prepared Nextest shards refused before executing a test.  Official consumers
# name the prepared runner directly.  Standalone use retains the normal shebang
# path and may compile as before.
function run_rust_script {
    local source=$1
    shift
    if [[ ${HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED:-0} == 1 ]]; then
        "$PREBUILT_RUST_SCRIPT" --force "$source" "$@"
    else
        "$source" "$@"
    fi
}

function cleanup_nextest_config {
    if [[ -n $nextest_config ]]; then
        retention_capture_config
        rm -f -- "$nextest_config"
        nextest_config=
    fi
}

function cleanup_cpu_measurement_dir {
    if [[ -n $cpu_measurement_dir ]]; then
        rm -rf -- "$cpu_measurement_dir"
        cpu_measurement_dir=
    fi
}

# Only standalone use can build. Official consumers require the recorded
# normal binary, independently of the Nextest test-harness binary of this name.
function build_cpu_wrapper {
    if [[ ${HERMIT_PREPARED_NEXTEST_REQUIRED:-0} == 1 ]]; then
        run_rust_script "$SCRIPT_DIR/nextest-binaries.rs" cpu-wrapper
    else
        run_rust_script "$SCRIPT_DIR/nextest-binaries.rs" build-cpu-wrapper
    fi
}

function invoke_nextest {
    local operation=$1
    local -a context_args=()
    shift
    if [[ ${HERMIT_PREPARED_NEXTEST_REQUIRED:-0} == 1 ]]; then
        [[ -z $calibration_launch ]] || context_args+=(--launch-proof "$calibration_launch")
        if [[ $operation == run && -n $nextest_budget_map ]]; then
            context_args+=(--budget-map "$nextest_budget_map" "$nextest_budget_sha")
        fi
        if [[ -n $retain_evidence ]]; then
            printf '%s\0' run_rust_script "$SCRIPT_DIR/nextest-binaries.rs" "$operation" "${context_args[@]}" --config-file "$nextest_config" "$@" >"$retain_evidence/$operation.dispatch.argv0" || retention_note_failure 'cannot record prepared dispatch'
        fi
        run_rust_script "$SCRIPT_DIR/nextest-binaries.rs" "$operation" "${context_args[@]}" --config-file "$nextest_config" "$@"
    else
        if [[ -n $retain_evidence ]]; then
            printf '%s\0' cargo nextest --config-file "$nextest_config" "$operation" "$@" >"$retain_evidence/$operation.dispatch.argv0" || retention_note_failure 'cannot record cargo dispatch'
        fi
        cargo nextest --config-file "$nextest_config" "$operation" "$@"
    fi
}

function configured_cpu_report_path {
    local count_path=${DAGRUN_TEST_COUNTS_PATH:--}
    if [[ ${HERMIT_NEXTEST_CPU_REPORT_PATH+x} ]]; then
        if [[ -z $HERMIT_NEXTEST_CPU_REPORT_PATH ]]; then
            printf 'run-nextest-counted: HERMIT_NEXTEST_CPU_REPORT_PATH must not be empty\n' >&2
            return 2
        fi
        printf '%s\n' "$HERMIT_NEXTEST_CPU_REPORT_PATH"
    elif [[ $count_path != - && -n $count_path ]]; then
        printf '%s.cpu.json\n' "$count_path"
    else
        printf '%s\n' -
    fi
}

function configured_wall_timeout_multiplier {
    printf '%s\n' "${HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER-1}"
}

function nextest_inventory_args {
    local -n output=$1
    shift
    output=()
    while (($#)); do
        case $1 in
            --)
                output+=("$@")
                return 0
                ;;
            -j|--jobs|--test-threads|--retries|--max-fail|--no-tests|\
                --failure-output|--success-output|--status-level|\
                --final-status-level|--message-format|--message-format-version)
                if (($# < 2)); then
                    printf 'run-nextest-counted: %s requires a value\n' "$1" >&2
                    return 2
                fi
                shift 2
                ;;
            -j?*|--jobs=*|--test-threads=*|--retries=*|--max-fail=*|--no-tests=*|\
                --failure-output=*|--success-output=*|--status-level=*|\
                --final-status-level=*|--message-format=*|--message-format-version=*)
                shift
                ;;
            --no-run|--fail-fast|--ff|--no-fail-fast|--nff|--no-capture|\
                --hide-progress-bar|--no-output-indent|--no-input-handler)
                shift
                ;;
            *)
                output+=("$1")
                shift
                ;;
        esac
    done
}

function emit_libtest_count {
    local events=$1 status=${2:-0} path=${DAGRUN_TEST_COUNTS_PATH:--}
    local attempts=${3-} binary_map=${4-} cpu_report=${5-} inventory=${6-}
    local writer_status=0
    [[ -z $retain_evidence ]] || retention_stage=parser
    local -a cpu_args=() inventory_args=()
    if [[ ! -s $events ]]; then
        printf 'run-nextest-counted: typed nextest event stream is empty\n' >&2
        return 2
    fi
    if [[ -n $attempts || -n $binary_map || -n $cpu_report ]]; then
        if [[ -z $attempts || -z $binary_map || -z $cpu_report ]]; then
            printf 'run-nextest-counted: CPU attempt directory, binary map, and report path must be supplied together\n' >&2
            return 2
        fi
        cpu_args=("$attempts" "$binary_map" "$cpu_report")
    fi
    if [[ -n $inventory ]]; then
        inventory_args=(--inventory "$inventory")
    fi
    run_rust_script "$RESULT_WRITER" "${inventory_args[@]}" "$events" "$status" "$path" "${cpu_args[@]}" || writer_status=$?
    retention_parser_status=$writer_status
    if ((writer_status != 0)); then
        printf 'run-nextest-counted: cannot derive typed test results from %s\n' "$events" >&2
        return "$writer_status"
    fi
}

function run_nextest {
    local events_log=$1 wall_multiplier=$2 cpu_wrapper=$3 attempts=$4 binary_map=$5
    local inventory=$6 cpu_report=$7
    local budget_map="$4/../resolved-budgets.json" budget_context="$4/../budget-context.json"
    local status count_status=0
    local -a inventory_arguments=()
    local -a launch_arguments=()
    shift 7

    if [[ $cpu_report != - && $cpu_report == "${DAGRUN_TEST_COUNTS_PATH:--}" ]]; then
        printf 'run-nextest-counted: CPU report path must differ from DAGRUN_TEST_COUNTS_PATH\n' >&2
        return 2
    fi

    [[ -z $retain_evidence ]] || retention_stage=config
    cleanup_nextest_config
    nextest_budget_map=
    nextest_budget_sha=
    nextest_config=$(mktemp "${TMPDIR:-/tmp}/hermit-nextest-config.XXXXXX.toml") || return $?
    if HERMIT_NEXTEST_CPU_WRAPPER_BIN="$cpu_wrapper" run_rust_script "$TIMEOUT_CONFIG_WRITER" \
        "$SCRIPT_DIR/../.config/nextest.toml" "$wall_multiplier" "$nextest_config"; then
        retention_config_status=0
    else
        retention_config_status=$?
        cleanup_nextest_config
        return 2
    fi

    nextest_inventory_args inventory_arguments "$@" || return $?
    if [[ -n $retain_evidence ]]; then
        retention_stage=list
        printf '%s\0' --color never --message-format json "${inventory_arguments[@]}" >"$retain_evidence/list-selection.argv0" || retention_note_failure 'cannot record list argv'
        retention_capture_config
    fi
    if HERMIT_NEXTEST_CPU_WRAPPER_BIN="$cpu_wrapper" invoke_nextest list --color never \
        --message-format json "${inventory_arguments[@]}" >"$inventory"; then
        retention_list_status=0
    else
        retention_list_status=$?
        printf 'run-nextest-counted: cannot obtain typed nextest binary inventory\n' >&2
        cleanup_nextest_config
        return 2
    fi
    [[ -z $retain_evidence ]] || retention_stage=binary-map
    if run_rust_script "$RESULT_WRITER" --write-binary-map "$inventory" "$binary_map"; then
        retention_map_status=0
    else
        retention_map_status=$?
        printf 'run-nextest-counted: cannot validate typed nextest binary inventory\n' >&2
        cleanup_nextest_config
        return 2
    fi

    # Prepared consumers can bind calibration to the exact checked build and
    # selected inventory. Standalone/unsupported populations keep the existing
    # path. Never infer a calibration context from unchecked executable names.
    if [[ ${HERMIT_PREPARED_NEXTEST_REQUIRED:-0} == 1 && $cpu_report != - ]]; then
        [[ -z $retain_evidence ]] || retention_stage=budget
        [[ -z $calibration_launch ]] || launch_arguments+=(--launch-proof "$calibration_launch")
        if run_rust_script "$SCRIPT_DIR/nextest-binaries.rs" budget-context \
            "${launch_arguments[@]}" "${inventory_arguments[@]}" >"$budget_context"; then
            retention_budget_context_status=0
        else
            retention_budget_context_status=$?
            cleanup_nextest_config
            return 2
        fi
        if HERMIT_NEXTEST_CPU_WRAPPER_BIN="$cpu_wrapper" run_rust_script "$TIMEOUT_CONFIG_WRITER" \
            --resolve "$SCRIPT_DIR/../.config/nextest.toml" "$wall_multiplier" "$nextest_config" \
            "$inventory" "$budget_context" "$budget_map" "$SCRIPT_DIR/../.config/nextest-budgets.json" \
            -- "$@"; then
            retention_budget_status=0
        else
            retention_budget_status=$?
            cleanup_nextest_config
            return 2
        fi
        if [[ -f $budget_map ]]; then
            local digest_line
            digest_line=$(sha256sum -- "$budget_map") || return $?
            nextest_budget_map=$budget_map
            nextest_budget_sha=${digest_line%% *}
            # Keep the exact digest-bound CPU/wall input on both success and
            # failure, beside the existing attempt report. Never replace an
            # earlier invocation's retained calibration evidence.
            if ! (set -o noclobber; cat -- "$budget_map" >"$cpu_report.budgets.json"); then
                cleanup_nextest_config
                return 2
            fi
        fi
    fi

    if [[ -n $retain_evidence ]]; then
        retention_stage=run
        printf '%s\0' --color never --message-format libtest-json-plus --message-format-version 0.1 "$@" >"$retain_evidence/run-selection.argv0" || retention_note_failure 'cannot record run argv'
        retention_capture_config
    fi
    set +e
    # nextest's stderr remains presentation. The versioned stdout event stream
    # is the only input to the aggregate count and per-test result adapter.
    HERMIT_NEXTEST_CPU_BINARY_MAP="$binary_map" \
        HERMIT_NEXTEST_CPU_RECORD_DIR="$attempts" \
        HERMIT_NEXTEST_CPU_WRAPPER_BIN="$cpu_wrapper" \
        NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 invoke_nextest run \
        --color never --message-format libtest-json-plus --message-format-version 0.1 \
        "$@" >"$events_log"
    status=$?
    retention_run_status=$status
    set -e
    cleanup_nextest_config

    emit_libtest_count "$events_log" "$status" "$attempts" "$binary_map" \
        "$cpu_report" "$inventory" || count_status=$?
    retention_parser_status=$count_status
    [[ -z $retain_evidence ]] || retention_stage=results-processed
    # Publication cannot erase a failure that the actual test runner observed.
    if ((status != 0)); then
        return "$status"
    fi
    return "$count_status"
}

function self_test {
    local scratch got expected status=0 wall_multiplier cpu_wrapper fixture_binary attempts
    local binary_map inventory launch_failure=0 inventory_wrong_name=0 ordinary_attempts
    scratch=$(mktemp -d)
    trap 'rm -rf "$scratch"' RETURN

    cpu_wrapper=$(build_cpu_wrapper) || return $?
    "$cpu_wrapper" --self-test || return $?

    printf '%s\n' \
        '{"type":"suite","event":"started","test_count":2,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        '{"type":"test","event":"started","name":"suite::suite$passes"}' \
        '{"type":"test","event":"ok","name":"suite::suite$passes","exec_time":0.1}' \
        '{"type":"test","event":"started","name":"suite::suite$recovers"}' \
        '{"type":"test","event":"ok","name":"suite::suite$recovers#2","exec_time":0.2}' \
        '{"type":"suite","event":"ok","passed":2,"failed":0,"ignored":0,"measured":0,"filtered_out":7,"exec_time":0.3,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        >"$scratch/events.jsonl"
    got=$(DAGRUN_TEST_COUNTS_PATH="$scratch/counts.json" \
        emit_libtest_count "$scratch/events.jsonl" 0)
    expected=$'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 7 filtered out'
    [[ $got == "$expected" ]] || return 1
    python3 - "$scratch/counts.json" <<'PYEOF' || return 1
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
    assert report == {
    "schema": 2,
    "executed_tests": 2,
    "filtered_tests": 7,
    "results": [
        {"id": "suite$passes", "result": "pass", "attempts": 1},
        {"id": "suite$recovers", "result": "pass", "attempts": 2},
    ],
    }
PYEOF

    sed 's/"filtered_out":7/"filtered_out":11/' \
        "$scratch/events.jsonl" >"$scratch/mutated-events.jsonl"
    got=$(DAGRUN_TEST_COUNTS_PATH="$scratch/mutated-counts.json" \
        emit_libtest_count "$scratch/mutated-events.jsonl" 0)
    expected=$'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 11 filtered out'
    [[ $got == "$expected" ]] || return 1
    python3 - "$scratch/mutated-counts.json" <<'PYEOF' || return 1
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
assert report["executed_tests"] == 2
assert report["filtered_tests"] == 11
PYEOF

    got=$(DAGRUN_TEST_COUNTS_PATH="$scratch/expected-counts.json" \
        NEXTEST_EXPECTED_EXECUTED=2 emit_libtest_count "$scratch/events.jsonl" 0)
    expected=$'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 7 filtered out'
    [[ $got == "$expected" ]] || return 1
    cmp -s "$scratch/expected-counts.json" "$scratch/counts.json" || return 1

    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/wrong-counts.json" \
        NEXTEST_EXPECTED_EXECUTED=3 emit_libtest_count "$scratch/events.jsonl" 0 \
        >"$scratch/wrong-count.stdout" 2>"$scratch/wrong-count.stderr" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'expected 3 tests to execute, saw 2' "$scratch/wrong-count.stderr" || return 1
    [[ ! -e $scratch/wrong-counts.json ]] || return 1

    status=0
    NEXTEST_EXPECTED_EXECUTED=unknown emit_libtest_count "$scratch/events.jsonl" 0 \
        >/dev/null 2>"$scratch/invalid-count.stderr" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'NEXTEST_EXPECTED_EXECUTED' "$scratch/invalid-count.stderr" || return 1

    [[ $(configured_wall_timeout_multiplier) == 1 ]] || return 1
    wall_multiplier=$(HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER=1.5 \
        configured_wall_timeout_multiplier)
    [[ $wall_multiplier == 1.5 ]] || return 1

    printf '%s\n' \
        '{"type":"suite","event":"started","test_count":1,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        '{"type":"test","event":"started","name":"suite::suite$fails"}' \
        '{"type":"test","event":"failed","name":"suite::suite$fails","exec_time":0.2}' \
        '{"type":"suite","event":"failed","passed":0,"failed":1,"ignored":0,"measured":0,"filtered_out":3,"exec_time":0.2,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
        >"$scratch/failed-events.jsonl"
    DAGRUN_TEST_COUNTS_PATH="$scratch/failed-counts.json" \
        emit_libtest_count "$scratch/failed-events.jsonl" 100 >/dev/null || return 1
    python3 - "$scratch/failed-counts.json" <<'PYEOF' || return 1
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    report = json.load(source)
assert report["results"] == [
    {"id": "suite$fails", "result": "fail", "attempts": 1},
]
PYEOF

    # Exercise the generated config, the real per-execution wrapper, and the
    # typed reconciliation path. A valid failed run must still return nextest's
    # original nonzero status.
    fixture_binary="$scratch/suite-0123456789abcdef"
    ln -s -- "$cpu_wrapper" "$fixture_binary"
    attempts="$scratch/wrapper-attempts"
    mkdir "$attempts"
    function cargo {
        [[ $1 == nextest && $2 == --config-file && -f $3 ]] || return 99
        cp -- "$3" "$scratch/scaled-nextest.toml"
        local operation=$4
        shift 4
        if [[ $operation == list ]]; then
            printf '%s\n' "$@" >"$scratch/list-arguments"
            jq -nc --arg executable "$fixture_binary" --argjson launch_failure "$launch_failure" \
                --argjson inventory_wrong_name "$inventory_wrong_name" '{
                "rust-suites": {
                    "suite": {
                        "package-name": "suite",
                        "binary-id": "suite",
                        "binary-name": "suite",
                        "kind": "lib",
                        "binary-path": $executable,
                        "testcases": (if $launch_failure == 1 then {} else {
                            (if $inventory_wrong_name == 1 then "substituted" else "failure" end):
                                {"ignored":false,"filter-match":{"status":"matches"}}
                        } end)
                    }
                }
            }'
            return
        fi
        [[ $operation == run ]] || return 99
        if ((launch_failure)); then
            printf '%s\n' \
                '{"type":"suite","event":"started","test_count":0,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
                '{"type":"suite","event":"failed","passed":0,"failed":0,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.0,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}'
            printf 'Summary [   0.010s] 12 tests run: 0 passed, 12 exec failed, 0 skipped\n' >&2
            return 100
        fi
        local wrapper_status=0
        HERMIT_NEXTEST_CPU_CONTROL=1 CARGO_PKG_NAME=suite \
            NEXTEST_RUN_ID=self-test-nextest NEXTEST_ATTEMPT=1 \
            HERMIT_NEXTEST_CPU_CONTROL_CWD="$PWD" \
            HERMIT_NEXTEST_CPU_CONTROL_SENTINEL=preserved \
            LD_LIBRARY_PATH="$PWD/deps" \
            setsid "$cpu_wrapper" "$fixture_binary" --exact failure --nocapture \
            >/dev/null 2>/dev/null || wrapper_status=$?
        [[ $wrapper_status == 23 ]] || return 99
        printf '%s\n' \
            '{"type":"suite","event":"started","test_count":1,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}' \
            '{"type":"test","event":"failed","name":"suite::suite$failure","exec_time":0.01}' \
            '{"type":"suite","event":"failed","passed":0,"failed":1,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.01,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}'
        printf 'Summary [   0.010s] 1 test run: 0 passed, 1 failed, 0 skipped\n' >&2
        return 100
    }
    set +e
    inventory="$scratch/wrapper-inventory.json"
    binary_map="$scratch/wrapper-binary-map.json"
    got=$(TMPDIR="$scratch" DAGRUN_TEST_COUNTS_PATH="$scratch/wrapper-counts.json" \
        run_nextest "$scratch/wrapper-events" "$wall_multiplier" "$cpu_wrapper" \
        "$attempts" "$binary_map" "$inventory" "$scratch/wrapper-cpu.json" \
        --profile ci -p suite -j 1 --retries 2 -- --skip skipme)
    status=$?
    set -e
    [[ $status == 100 ]] || return 1
    [[ $got == *$'running 1 tests\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 filtered out' ]] || return 1
    [[ $got != *'test result: ok.'* ]] || return 1
    printf '%s\n' --color never --message-format json --profile ci -p suite \
        -- --skip skipme >"$scratch/expected-list-arguments"
    cmp -s "$scratch/expected-list-arguments" "$scratch/list-arguments" || return 1
    [[ $(<"$scratch/wrapper-counts.json") == \
        '{"executed_tests":1,"filtered_tests":0,"results":[{"attempts":1,"id":"suite$failure","result":"fail"}],"schema":2}' ]] || return 1
    jq -e '
        .schema == 3 and .run_id == "self-test-nextest" and
        (.attempts | length) == 1 and
        .attempts[0].identity == {package:"suite", binary:"suite", test:"failure", attempt:1} and
        .attempts[0].completion == {kind:"exit", code:23} and
        .attempts[0].cpu_source == "wait4-subtree"
    ' "$scratch/wrapper-cpu.json" >/dev/null || return 1
    grep -q 'period = "86s"' "$scratch/scaled-nextest.toml" || return 1
    [[ $(grep -c '^run-wrapper = "hermit-per-test-cpu"$' \
        "$scratch/scaled-nextest.toml") == 1 ]] || return 1
    grep -q '^experimental = \["wrapper-scripts"\]$' \
        "$scratch/scaled-nextest.toml" || return 1
    grep -q '^target-runner = "within-wrapper"$' \
        "$scratch/scaled-nextest.toml" || return 1
    # This is the command Nextest receives, not the fixture's direct wrapper call.
    grep -Fq '"--cpu-timeout-usec", "22000000", "--termination-grace-ms", "2000", "--"' \
        "$scratch/scaled-nextest.toml" || return 1
    if compgen -G "$scratch/hermit-nextest-config.*.toml" >/dev/null; then
        printf 'run-nextest-counted: temporary nextest config leaked\n' >&2
        return 1
    fi

    # Synthetic mutation: the map and counts still agree, but inventory names
    # do not. This must reach the result writer through run_nextest and refuse;
    # omitting --inventory would wrongly publish the ordinary failed result.
    inventory_wrong_name=1
    mkdir "$scratch/attribution-attempts"
    status=0
    TMPDIR="$scratch" DAGRUN_TEST_COUNTS_PATH="$scratch/attribution-counts.json" \
        run_nextest "$scratch/attribution-events" "$wall_multiplier" "$cpu_wrapper" \
        "$scratch/attribution-attempts" "$binary_map" "$inventory" "$scratch/attribution-cpu.json" \
        --profile ci -p suite -j 1 --retries 2 -- --skip skipme \
        >"$scratch/attribution.stdout" 2>"$scratch/attribution.stderr" || status=$?
    [[ $status == 100 ]] || return 1
    [[ ! -e $scratch/attribution-counts.json && ! -e $scratch/attribution-cpu.json ]] || return 1
    grep -q '0 eligible matches' "$scratch/attribution.stderr" || return 1
    inventory_wrong_name=0

    # Preserve the original zero-terminal launch failure separately from the
    # executed exit23 control above. Stderr's twelve failures are not test rows.
    launch_failure=1
    ordinary_attempts=$attempts
    attempts="$scratch/launch-attempts"
    mkdir "$attempts"
    status=0
    got=$(TMPDIR="$scratch" DAGRUN_TEST_COUNTS_PATH="$scratch/launch-counts.json" \
        run_nextest "$scratch/launch-events" "$wall_multiplier" "$cpu_wrapper" \
        "$attempts" "$binary_map" "$inventory" "$scratch/launch-cpu.json" \
        --profile ci -p suite -j 1 --retries 2 -- --skip skipme) || status=$?
    [[ $status == 100 ]] || return 1
    [[ $got == *$'running 0 tests\ntest result: FAILED. 0 passed; 0 failed; 0 ignored; 0 filtered out' ]] || return 1
    [[ $got != *'test result: ok.'* ]] || return 1
    [[ $(<"$scratch/launch-counts.json") == \
        '{"executed_tests":0,"filtered_tests":0,"results":[],"schema":2}' ]] || return 1
    jq -e '. == {schema:3, run_id:null, attempts:[]}' "$scratch/launch-cpu.json" >/dev/null || return 1
    cmp -s "$scratch/expected-list-arguments" "$scratch/list-arguments" || return 1
    if compgen -G "$scratch/hermit-nextest-config.*.toml" >/dev/null; then return 1; fi

    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/launch-wrong-count.json" NEXTEST_EXPECTED_EXECUTED=1 \
        emit_libtest_count "$scratch/launch-events" 100 "$attempts" "$binary_map" \
        "$scratch/launch-wrong-cpu.json" >"$scratch/launch-wrong.stdout" 2>"$scratch/launch-wrong.stderr" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'expected 1 tests to execute, saw 0' "$scratch/launch-wrong.stderr" || return 1
    # The refusal must carry the outcome it already knows. Withholding the typed
    # report is deliberate and asserted on the next line, so this message is the
    # only place the run can say whether the tests it DID run passed. Without it
    # an all-green population drift and a failing one read identically.
    grep -q 'of which 0 passed and 0 failed' "$scratch/launch-wrong.stderr" || return 1
    [[ ! -e $scratch/launch-wrong-count.json && ! -e $scratch/launch-wrong-cpu.json ]] || return 1
    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/failed-wrong-count.json" NEXTEST_EXPECTED_EXECUTED=2 \
        emit_libtest_count "$scratch/wrapper-events" 100 "$ordinary_attempts" "$binary_map" \
        "$scratch/failed-wrong-cpu.json" >"$scratch/failed-wrong.stdout" 2>"$scratch/failed-wrong.stderr" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'expected 2 tests to execute, saw 1' "$scratch/failed-wrong.stderr" || return 1
    grep -q 'of which 0 passed and 1 failed' "$scratch/failed-wrong.stderr" || return 1
    [[ ! -e $scratch/failed-wrong-count.json && ! -e $scratch/failed-wrong-cpu.json ]] || return 1
    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/launch-stray-count.json" \
        emit_libtest_count "$scratch/launch-events" 100 "$ordinary_attempts" "$binary_map" \
        "$scratch/launch-stray-cpu.json" >"$scratch/launch-stray.stdout" 2>"$scratch/launch-stray.stderr" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'unexpected=' "$scratch/launch-stray.stderr" || return 1
    [[ ! -e $scratch/launch-stray-count.json && ! -e $scratch/launch-stray-cpu.json ]] || return 1
    unset -f cargo

    printf '%s\n' \
        '{"type":"test","event":"future","name":"suite::suite$case"}' \
        >"$scratch/unknown-event.jsonl"
    status=0
    DAGRUN_TEST_COUNTS_PATH="$scratch/refused.json" \
        emit_libtest_count "$scratch/unknown-event.jsonl" 0 \
        >/dev/null 2>"$scratch/refusal" || status=$?
    [[ $status == 2 ]] || return 1
    grep -q 'unsupported test event "future"' "$scratch/refusal" || return 1
    [[ ! -e $scratch/refused.json ]] || return 1

    printf 'run-nextest-counted: self-test PASS (typed counts, executed failure, zero-terminal launch failure, CPU reconciliation, refusal controls)\n'
}

function retention_self_test {
    (
        local scratch fault case_dir status expected original
        scratch=$(mktemp -d) || exit $?
        trap 'rm -rf -- "$scratch"' EXIT
        for fault in config list binary-map budget-context budget-resolve runner runner126 runner127 parser success; do
            case_dir="$scratch/$fault"
            mkdir -- "$case_dir" || exit $?
            status=0
            (
                events_log="$case_dir/raw"
                cpu_measurement_dir="$case_dir/cpu"
                cpu_attempt_records="$cpu_measurement_dir/attempts"
                cpu_inventory="$cpu_measurement_dir/inventory.json"
                cpu_binary_map="$cpu_measurement_dir/binary-map.json"
                skid_retry_record="$cpu_measurement_dir/skid-retries.log"
                cpu_report="$case_dir/cpu-report.json"
                DAGRUN_TEST_COUNTS_PATH="$case_dir/counts.json"
                calibration_launch=
                nextest_config=
                TMPDIR="$case_dir"
                HERMIT_PREPARED_NEXTEST_REQUIRED=1
                mkdir -p -- "$cpu_attempt_records" || exit $?
                : >"$events_log"
                printf 'partial raw attempt; not a valid producer fixture\n' >"$cpu_attempt_records/.partial"
                printf 'synthetic marker-name attempt\n' >"$cpu_attempt_records/CAPTURED"
                function run_rust_script {
                    local source=$1
                    shift
                    if [[ $source == "$TIMEOUT_CONFIG_WRITER" ]]; then
                        if [[ ${1:-} == --resolve ]]; then
                            [[ $fault != budget-resolve ]] || return 35
                            printf 'synthetic resolved config\n' >"$4"
                        else
                            printf 'synthetic config\n' >"$3"
                            [[ $fault != config ]] || return 31
                        fi
                    elif [[ $source == "$RESULT_WRITER" ]]; then
                        if [[ ${1:-} == --write-binary-map ]]; then
                            [[ $fault != binary-map ]] || return 33
                            printf 'synthetic binary map\n' >"$3"
                        else
                            [[ $fault != parser ]] || return 7
                            # No fake typed result is published by the stub.
                        fi
                    elif [[ $source == "$SCRIPT_DIR/nextest-binaries.rs" && ${1:-} == budget-context ]]; then
                        [[ $fault != budget-context ]] || return 34
                        printf 'null\n'
                    else
                        return 99
                    fi
                }
                function invoke_nextest {
                    if [[ $1 == list ]]; then
                        printf 'partial synthetic inventory\n'
                        [[ $fault != list ]] || return 32
                    else
                        printf 'raw synthetic events, not valid libtest JSON\n'
                        case $fault in runner) return 100 ;; runner126) return 126 ;; runner127) return 127 ;; esac
                    fi
                }
                retention_begin "$case_dir/evidence" --profile ci -p synthetic || exit $?
                status=0
                run_nextest "$events_log" 1 synthetic-wrapper "$cpu_attempt_records" \
                    "$cpu_binary_map" "$cpu_inventory" "$cpu_report" \
                    --profile ci -p synthetic || status=$?
                exit "$status" # exercise the real retention EXIT and ordinary cleanup
            ) || status=$?
            case $fault in runner) expected=100 ;; runner126) expected=126 ;; runner127) expected=127 ;; parser) expected=7 ;; success) expected=0 ;; *) expected=2 ;; esac
            [[ $status == "$expected" ]] || exit 1
            [[ ! -e $case_dir/raw && ! -e $case_dir/cpu ]] || exit 1
            # Every raw attempt survives, even incomplete/malformed bytes.
            [[ $(cat "$case_dir/evidence/attempts/.partial") == 'partial raw attempt; not a valid producer fixture' ]] || exit 1
            [[ -f $case_dir/evidence/typed-result.observed.json.absent ]] || exit 1
            jq -e --argjson expected "$expected" '.final_status==$expected and .producer_identity=="unattested" and .capture_status==0' \
                "$case_dir/evidence/status.json" >/dev/null || exit 1
            case $fault in
                config) jq -e '.config_status=="31" and .nextest_status=="not-run"' "$case_dir/evidence/status.json" >/dev/null || exit 1 ;;
                list) jq -e '.list_status=="32" and .nextest_status=="not-run"' "$case_dir/evidence/status.json" >/dev/null || exit 1 ;;
                binary-map) jq -e '.map_status=="33" and .nextest_status=="not-run"' "$case_dir/evidence/status.json" >/dev/null || exit 1 ;;
                budget-context) jq -e '.budget_context_status=="34" and .nextest_status=="not-run"' "$case_dir/evidence/status.json" >/dev/null || exit 1 ;;
                budget-resolve) jq -e '.budget_status=="35" and .nextest_status=="not-run"' "$case_dir/evidence/status.json" >/dev/null || exit 1 ;;
                runner) jq -e '.nextest_status=="100" and .parser_status=="0"' "$case_dir/evidence/status.json" >/dev/null || exit 1 ;;
                parser) jq -e '.nextest_status=="0" and .parser_status=="7"' "$case_dir/evidence/status.json" >/dev/null || exit 1 ;;
                success) jq -e '.nextest_status=="0" and .parser_status=="0" and .capture_io_failed==0' "$case_dir/evidence/status.json" >/dev/null || exit 1 ;;
            esac
            [[ -f $case_dir/evidence/CAPTURED ]] || exit 1
            grep -q '  ./attempts/CAPTURED$' "$case_dir/evidence/SHA256SUMS" || exit 1
            if [[ $fault == success ]]; then
                printf 'raw synthetic events, not valid libtest JSON\n' >"$case_dir/expected-raw"
                cmp -s -- "$case_dir/expected-raw" "$case_dir/evidence/events.jsonl" || exit 1
            fi
            (cd -- "$case_dir/evidence" && sha256sum --check SHA256SUMS >/dev/null) || exit 1
            if compgen -G "$case_dir/hermit-nextest-config.*.toml" >/dev/null; then exit 1; fi
        done
        mkdir "$scratch/occupied" || exit $?
        printf sentinel >"$scratch/occupied/sentinel"
        ln -s occupied "$scratch/alias" || exit $?
        printf sentinel >"$scratch/occupied-file"
        ln -s missing "$scratch/dangling" || exit $?
        for case_dir in "$scratch/occupied" "$scratch/occupied-file" "$scratch/alias" \
            "$scratch/dangling" "$scratch/missing-parent/new" "$scratch/trailing/" relative; do
            (retention_begin "$case_dir") >/dev/null 2>&1 && exit 1
        done
        [[ $(cat "$scratch/occupied/sentinel") == sentinel ]] || exit 1
        for expected in 2 7 100 126 127; do
            (
                events_log="$scratch/io-raw-$expected"
                printf real-bytes >"$events_log"
                cpu_measurement_dir="$scratch/io-cpu-$expected"
                cpu_attempt_records="$cpu_measurement_dir/attempts"
                mkdir -p "$cpu_attempt_records" || exit $?
                cpu_inventory= cpu_binary_map= skid_retry_record=
                cpu_report=- DAGRUN_TEST_COUNTS_PATH=- calibration_launch= nextest_config=
                retention_begin "$scratch/io-$expected" || exit $?
                trap - EXIT
                mkdir "$scratch/io-$expected/events.jsonl" || exit $?
                retention_run_status=$expected
                [[ $expected != 2 ]] || retention_run_status=0
                status=0
                retention_finish "$retention_run_status" || status=$?
                [[ $status == "$expected" && ! -e $scratch/io-$expected/CAPTURED ]] || exit 1
                jq -e --argjson expected "$expected" '.capture_status==2 and .final_status==$expected' \
                    "$scratch/io-$expected/status.json" >/dev/null || exit 1
            ) || exit $?
        done
        # Synthetic late-I/O and missing-evidence controls exercise the actual
        # EXIT path. Stubs/bytes here are not a producer qualification fixture.
        for fault in status checksum marker missing-raw missing-attempts; do
            for original in 0 100; do
                case_dir="$scratch/late-$fault-$original"
                mkdir "$case_dir" || exit $?
                status=0
                (
                    events_log="$case_dir/raw"
                    cpu_measurement_dir="$case_dir/cpu"
                    cpu_attempt_records="$cpu_measurement_dir/attempts"
                    cpu_inventory= cpu_binary_map= skid_retry_record=
                    cpu_report=- DAGRUN_TEST_COUNTS_PATH=- calibration_launch= nextest_config=
                    mkdir -p "$cpu_attempt_records" || exit $?
                    printf 'synthetic raw bytes\n' >"$events_log"
                    # Names that resemble top-level markers must still be hashed.
                    printf 'synthetic attempt\n' >"$cpu_attempt_records/CAPTURED"
                    retention_begin "$case_dir/evidence" || exit $?
                    retention_run_status=$original
                    case $fault in
                        status) mkdir "$case_dir/evidence/status.json" ;;
                        checksum) mkdir "$case_dir/evidence/SHA256SUMS" ;;
                        marker) mkdir "$case_dir/evidence/.CAPTURED.pending" ;;
                        missing-raw) rm "$events_log" ;;
                        missing-attempts) rm -rf "$cpu_attempt_records" ;;
                    esac
                    exit "$original"
                ) || status=$?
                expected=$original
                ((expected != 0)) || expected=2
                [[ $status == "$expected" && ! -e $case_dir/evidence/CAPTURED ]] || exit 1
                [[ ! -e $case_dir/raw && ! -e $case_dir/cpu ]] || exit 1
            done
        done
        printf 'run-nextest-counted: synthetic capture controls passed (bytes, cleanup, and status only)\n'
    )
}

# Synthetic admission controls: the real CLI must refuse before any helper.
# Fake PATH tools only detect an accidental launch; they never qualify a producer.
function retention_overlap_self_test {
    (
        local scratch case_name case_dir destination count_path cpu_path status
        scratch=$(mktemp -d) || exit $?
        trap 'rm -rf -- "$scratch"' EXIT
        mkdir "$scratch/tools" || exit $?
        printf '#!/usr/bin/env bash\nprintf "unexpected helper\\n" >>"$CAPTURE_LAUNCH_SENTINEL"\nexit 99\n' >"$scratch/tools/rust-script"
        cp "$scratch/tools/rust-script" "$scratch/tools/cargo" || exit $?
        chmod +x "$scratch/tools/rust-script" "$scratch/tools/cargo" || exit $?
        for case_name in count-status count-marker count-equal cpu-status cpu-marker cpu-equal default-cpu \
            counts-recovery cpu-launch cpu-budgets relative symlink-parent ancestor-output existing; do
            case_dir="$scratch/$case_name"
            mkdir "$case_dir" || exit $?
            destination="$case_dir/capture"
            count_path="$case_dir/counts.json"
            cpu_path="$case_dir/cpu.json"
            printf 'caller counts\n' >"$count_path"
            printf 'caller CPU\n' >"$cpu_path"
            case $case_name in
                count-status) count_path="$destination/status.json" ;;
                count-marker) count_path="$destination/CAPTURED" ;;
                count-equal) count_path="$destination" ;;
                cpu-status) cpu_path="$destination/status.json" ;;
                cpu-marker) cpu_path="$destination/CAPTURED" ;;
                cpu-equal) cpu_path="$destination" ;;
                default-cpu) destination="$count_path.cpu.json" ;;
                counts-recovery) destination="$count_path.publication-recovery" ;;
                cpu-launch) destination="$cpu_path.launch.json" ;;
                cpu-budgets) destination="$cpu_path.budgets.json" ;;
                relative) count_path='./capture/../capture/status.json' ;;
                symlink-parent)
                    ln -s "$case_dir" "$case_dir/alias" || exit $?
                    count_path='alias/capture/status.json'
                    ;;
                ancestor-output) count_path="$case_dir" ;;
                existing)
                    mkdir "$destination" || exit $?
                    printf 'caller status\n' >"$destination/status.json"
                    printf 'caller marker\n' >"$destination/CAPTURED"
                    count_path="$destination/status.json"
                    ;;
            esac
            status=0
            (
                cd "$case_dir" || exit $?
                export DAGRUN_TEST_COUNTS_PATH="$count_path"
                export HERMIT_NEXTEST_CPU_REPORT_PATH="$cpu_path"
                [[ $case_name != default-cpu ]] || unset HERMIT_NEXTEST_CPU_REPORT_PATH
                export HERMIT_PREPARED_NEXTEST_REQUIRED=0 HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=0
                export PATH="$scratch/tools:$PATH" CAPTURE_LAUNCH_SENTINEL="$case_dir/launched"
                bash "$SCRIPT_DIR/run-nextest-counted.sh" --capture-evidence "$destination" --profile ci
            ) >"$case_dir/stdout" 2>"$case_dir/stderr" || status=$?
            [[ $status == 2 && ! -e $case_dir/launched ]] || exit 1
            grep -q 'output publication .* overlaps capture directory' "$case_dir/stderr" || exit 1
            [[ $(cat "$case_dir/counts.json") == 'caller counts' ]] || exit 1
            [[ $(cat "$case_dir/cpu.json") == 'caller CPU' ]] || exit 1
            if [[ $case_name == existing ]]; then
                [[ $(cat "$destination/status.json") == 'caller status' &&
                    $(cat "$destination/CAPTURED") == 'caller marker' ]] || exit 1
                [[ $(find "$destination" -mindepth 1 -maxdepth 1 -type f | wc -l) == 2 ]] || exit 1
            else
                [[ ! -e $destination && ! -L $destination ]] || exit 1
            fi
        done
        # An adjacent output with a shared string prefix is not containment.
        # Relative output parents may be created by the existing CPU writer.
        (
            cd "$scratch" || exit $?
            DAGRUN_TEST_COUNTS_PATH=./capture-sibling/status.json \
                HERMIT_NEXTEST_CPU_REPORT_PATH=./new-output-parent/cpu.json \
                retention_check_publication_overlap "$scratch/capture" || exit $?
            DAGRUN_TEST_COUNTS_PATH=- HERMIT_NEXTEST_CPU_REPORT_PATH=- \
                retention_check_publication_overlap "$scratch/capture" || exit $?
            [[ ! -e $scratch/capture ]] || exit 1
        ) || exit $?
        printf 'run-nextest-counted: synthetic capture publication-overlap controls passed\n'
    )
}

# Opt-in only; parse before calibration options. All original arguments are kept.
retention_destination=
if [[ ${1:-} == --capture-evidence ]]; then
    [[ $# -ge 2 && $2 == /* ]] || { printf 'run-nextest-counted: --capture-evidence requires a NEW canonical absolute directory under an existing writable parent; use --help\n' >&2; exit 2; }
    retention_destination=$2
    shift 2
fi
if [[ ${1:-} == --help || ${1:-} == -h ]]; then
    printf '%s\n' 'usage: ci/run-nextest-counted.sh [--capture-evidence NEW_ABSOLUTE_DIRECTORY] [--calibration-host | --calibration-launch-proof PATH] NEXTEST_ARGS...' \
        '--capture-evidence saves exact available bytes and explicit absences before normal cleanup into a NEW canonical absolute directory (parent must exist).' \
        'Capture success preserves runner status, including 0; it does not qualify tests or attest list/run producer identity.' \
        'Capture I/O/integrity failure makes an otherwise-successful run return 2; an existing failure keeps its status. CAPTURED plus valid SHA256SUMS means bytes saved only.' \
        'Place --capture-evidence before calibration and Nextest arguments; capture is not supported with --self-test.' \
        'Capture must be separate from counts/recovery outputs and the configured or default CPU output plus .launch.json/.budgets.json sidecars; relative output paths use the current directory.' \
        'The maintained native host boundary may capture actual resources with --calibration-host.' \
        'Pinned callers use the existing wrapper-provided read-only launch proof.' \
        'Missing evidence keeps defaults; malformed or changed evidence refuses. Launch observation never compiles.'
    exit 0
fi
if [[ ${1:-} == --calibration-host ]]; then
    calibration_host=true
    shift
elif [[ ${1:-} == --calibration-launch-proof ]]; then
    [[ $# -ge 2 && $2 == /* ]] || { printf 'run-nextest-counted: --calibration-launch-proof requires an absolute path; use --help\n' >&2; exit 2; }
    calibration_launch=$2
    shift 2
fi
if [[ ${1:-} == --self-test ]]; then
    [[ -z $retention_destination ]] || { printf 'run-nextest-counted: --capture-evidence cannot wrap --self-test; omit capture for synthetic controls, or supply normal Nextest arguments; use --help\n' >&2; exit 2; }
    self_test || exit $?
    retention_self_test
    retention_self_test_status=$?
    ((retention_self_test_status == 0)) || exit "$retention_self_test_status"
    retention_overlap_self_test
    exit
fi

events_log=
cpu_report=
if [[ -n $retention_destination ]]; then
    retention_begin "$retention_destination" "$@" || {
        retention_init_status=$?
        retention_note_failure "initialization refused or failed: status $retention_init_status"
        exit "$retention_init_status"
    }
fi
events_log=$(mktemp) || exit $?
cpu_measurement_dir=$(mktemp -d "${TMPDIR:-/tmp}/hermit-nextest-cpu.XXXXXX") || exit $?
cpu_attempt_records="$cpu_measurement_dir/attempts"
cpu_inventory="$cpu_measurement_dir/inventory.json"
cpu_binary_map="$cpu_measurement_dir/binary-map.json"
mkdir "$cpu_attempt_records" || exit $?
if [[ -z $retain_evidence ]]; then
    trap 'cleanup_nextest_config; cleanup_cpu_measurement_dir; rm -f "$events_log"' EXIT
fi
retention_stage=cpu-wrapper
if cpu_wrapper=$(build_cpu_wrapper); then
    retention_cpu_wrapper_status=0
else
    retention_cpu_wrapper_status=$?
    exit "$retention_cpu_wrapper_status"
fi
retention_stage=launch
cpu_report=$(configured_cpu_report_path) || exit $?
if [[ -n $retain_evidence && $cpu_report != - ]]; then
    retention_copy "$cpu_report" cpu-report.before.observed.json || :
    retention_copy "$cpu_report.launch.json" launch-publication.before.observed.json || :
    retention_copy "$cpu_report.budgets.json" budget-publication.before.observed.json || :
fi
if "$calibration_host"; then
    capture_manifest=${HERMIT_RUST_SCRIPT_ARTIFACT_ROOT:-$SCRIPT_DIR/../target/ci/rust-scripts}/manifest.tsv
    if [[ -e $capture_manifest || -L $capture_manifest ]]; then
        resolve_status=0
        capture_binary=$("$PREBUILT_RUST_SCRIPT" --resolve-optional --force "$SCRIPT_DIR/nextest-binaries.rs") || resolve_status=$?
        if ((resolve_status != 0 && resolve_status != 3)); then exit "$resolve_status"; fi
        capture_status=2
        if ((resolve_status == 0)); then
            capture_status=0
            capture_version=$("$capture_binary" capture-launch --probe) || capture_status=$?
        fi
        case $capture_status in
            0)
                [[ $capture_version == nextest-launch-observation-v2 ]] || { printf 'nextest budgets: malformed prepared capture capability; refresh build.rust_scripts\n' >&2; exit 2; }
                calibration_launch="$cpu_measurement_dir/launch.json"
                "$capture_binary" capture-launch --host --output "$calibration_launch" || exit $?
                ;;
            2) printf 'nextest budgets: prepared capture capability unavailable; unchanged defaults\n' >&2 ;;
            *) exit "$capture_status" ;;
        esac
    else
        printf 'nextest budgets: prepared host capture unavailable; unchanged defaults\n' >&2
    fi
fi
if [[ -n $calibration_launch && ( -e $calibration_launch || -L $calibration_launch ) && $cpu_report != - ]]; then
    [[ -f $calibration_launch && ! -L $calibration_launch ]] || { printf 'nextest budgets: launch proof must be a regular non-symlink file\n' >&2; exit 2; }
    (set -o noclobber; head -c 262145 -- "$calibration_launch" >"$cpu_report.launch.json") || exit $?
    # Preserve the wrapper mount as authority; this copy only retains evidence.
fi
# Hermit-running tests retry a typed PMU skid-overshoot refusal and append
# one line per retry here (hermit-cli/tests/common/skid_overshoot.rs). Report
# the count even when every test passed, so a rising skid rate is visible in
# the validate log rather than hidden by the retries.
skid_retry_record="$cpu_measurement_dir/skid-retries.log"
: >"$skid_retry_record" || exit $?
export HERMIT_SKID_RETRY_RECORD=$skid_retry_record
nextest_status=0
run_nextest "$events_log" "$(configured_wall_timeout_multiplier)" "$cpu_wrapper" \
    "$cpu_attempt_records" "$cpu_binary_map" "$cpu_inventory" "$cpu_report" "$@" \
    || nextest_status=$?
skid_retries=$(wc -l <"$skid_retry_record") || exit $?
printf 'run-nextest-counted: skid-overshoot retries: %d\n' "$skid_retries" >&2
if ((skid_retries > 0)); then
    sed 's/^/run-nextest-counted: skid-overshoot retry: /' "$skid_retry_record" >&2
fi
exit "$nextest_status"
