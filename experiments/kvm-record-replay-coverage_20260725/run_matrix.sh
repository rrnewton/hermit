#!/usr/bin/env bash

set -uo pipefail
ulimit -c 0

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 2
}

normalize_stderr() {
    python3 -c 'from pathlib import Path; import sys; data=Path(sys.argv[1]).read_bytes().split(b"\nRECORDING COMPLETE!", 1)[0]; Path(sys.argv[2]).write_bytes(data)' "$1" "$2"
}

script_dir=$(cd "$(dirname "$0")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
hermit_bin=${HERMIT_BIN:-$repo_root/target/release/hermit}
phase_timeout=${PHASE_TIMEOUT_SECONDS:-60}
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
artifact_root=${ARTIFACT_ROOT:-$script_dir/artifacts/$timestamp}
results=${RESULTS_FILE:-$script_dir/results.tsv}
metadata=${METADATA_FILE:-$script_dir/metadata.txt}

[[ -x $hermit_bin ]] || fail "Hermit binary is not executable: $hermit_bin"
[[ $phase_timeout =~ ^[1-9][0-9]*$ ]] || fail "PHASE_TIMEOUT_SECONDS must be a positive integer"
[[ ! -e $artifact_root ]] || fail "artifact directory already exists: $artifact_root"
for command in cmp cut date git jq python3 sha256sum timeout tr; do
    command -v "$command" >/dev/null || fail "required command not found: $command"
done

mkdir -p "$artifact_root"
cd "$repo_root" || fail "cannot enter repository root"
export LC_ALL=C

{
    printf 'schema_version=1\n'
    printf 'started_at_utc=%s\n' "$timestamp"
    printf 'repository_commit=%s\n' "$(git rev-parse HEAD)"
    printf 'repository_branch=%s\n' "$(git branch --show-current)"
    printf 'hermit=%s\n' "$hermit_bin"
    printf 'hermit_sha256=%s\n' "$(sha256sum "$hermit_bin" | awk '{print $1}')"
    printf 'phase_timeout_seconds=%s\n' "$phase_timeout"
    printf 'host_kernel=%s\n' "$(uname -srmo)"
    printf 'cpu_model=%s\n' "$(awk -F ': ' '/model name/{print $2; exit}' /proc/cpuinfo)"
} >"$metadata"

printf 'backend\tprogram\trecord\treplay\toutput_match\texit_match\trecord_status\treplay_status\trecord_ms\treplay_ms\tcommand\tfailure\n' >"$results"

rows=0
passes=0

run_case() {
    local backend=$1
    local name=$2
    shift 2
    local args=("$@")
    local case_dir=$artifact_root/$backend/$name
    local data_dir=$case_dir/data
    local command_line id recording_dir
    local record_status replay_status record_start replay_start record_ms replay_ms
    local record_result=fail replay_result=not-run output_match=not-run exit_match=not-run
    local failure=-

    mkdir -p "$case_dir" "$data_dir"
    printf -v command_line '%q ' "${args[@]}"
    command_line=${command_line% }

    record_start=$(date +%s%N)
    timeout --signal=TERM "$phase_timeout"s \
        "$hermit_bin" --backend "$backend" --log off record \
        --data-dir "$data_dir" --record-timeout "$phase_timeout" -- "${args[@]}" \
        >"$case_dir/record.stdout" 2>"$case_dir/record.stderr"
    record_status=$?
    record_ms=$((($(date +%s%N) - record_start) / 1000000))
    normalize_stderr "$case_dir/record.stderr" "$case_dir/record.guest.stderr"

    id=
    [[ -f $data_dir/last ]] && id=$(tr -d '\n' <"$data_dir/last")
    recording_dir=$data_dir/$id
    if [[ $record_status -eq 0 && -n $id && -f $recording_dir/metadata.json ]]; then
        record_result=pass
        if [[ $(jq -r .backend "$recording_dir/metadata.json") != "$backend" ]]; then
            record_result=wrong-backend
        fi
    fi

    replay_status=not-run
    replay_ms=0
    : >"$case_dir/replay.stdout"
    : >"$case_dir/replay.stderr"
    : >"$case_dir/replay.guest.stderr"
    if [[ -n $id && -f $recording_dir/metadata.json ]]; then
        replay_start=$(date +%s%N)
        timeout --signal=TERM "$phase_timeout"s \
            "$hermit_bin" --backend "$backend" --log off replay --autopilot \
            --data-dir "$data_dir" "$id" \
            >"$case_dir/replay.stdout" 2>"$case_dir/replay.stderr"
        replay_status=$?
        replay_ms=$((($(date +%s%N) - replay_start) / 1000000))
        normalize_stderr "$case_dir/replay.stderr" "$case_dir/replay.guest.stderr"
        [[ $replay_status -eq 0 ]] && replay_result=pass || replay_result=fail
        [[ $replay_status -eq $record_status ]] && exit_match=pass || exit_match=fail
        if cmp -s "$case_dir/record.stdout" "$case_dir/replay.stdout" &&
            cmp -s "$case_dir/record.guest.stderr" "$case_dir/replay.guest.stderr"; then
            output_match=pass
        else
            output_match=fail
        fi
    fi

    if [[ $record_result != pass ]]; then
        failure=$(tr '\n\t' '  ' <"$case_dir/record.stderr" | cut -c1-180)
    elif [[ $replay_result != pass ]]; then
        failure=$(tr '\n\t' '  ' <"$case_dir/replay.stderr" | cut -c1-180)
    elif [[ $output_match != pass || $exit_match != pass ]]; then
        failure=observable-mismatch
    fi
    [[ -n $failure ]] || failure=-

    rows=$((rows + 1))
    if [[ $record_result == pass && $replay_result == pass &&
        $output_match == pass && $exit_match == pass ]]; then
        passes=$((passes + 1))
    fi

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$backend" "$name" "$record_result" "$replay_result" "$output_match" \
        "$exit_match" "$record_status" "$replay_status" "$record_ms" "$replay_ms" \
        "$command_line" "$failure" >>"$results"
    printf '%-6s %-8s record=%-13s replay=%-7s output=%-7s %6sms/%6sms\n' \
        "$backend" "$name" "$record_result" "$replay_result" "$output_match" \
        "$record_ms" "$replay_ms"
}

run_program() {
    local name=$1
    shift
    run_case ptrace "$name" "$@"
    run_case kvm "$name" "$@"
}

run_program echo /bin/echo hermit-record-replay
run_program seq /usr/bin/seq 10
run_program cat /bin/cat README.md
run_program wc /usr/bin/wc -c README.md
run_program head /usr/bin/head -n 3 README.md
run_program base64 /usr/bin/base64 README.md
run_program id /usr/bin/id -u
run_program lua /usr/bin/lua -e 'print(42)'
run_program perl /usr/bin/perl -e 'print 42, chr(10)'
run_program awk /usr/bin/awk 'BEGIN { print 42 }'
run_program bc /bin/bash -c 'printf "6*7\n" | /usr/bin/bc'
run_program sqlite3 /usr/bin/sqlite3 :memory: \
    'CREATE TABLE values_under_test(value INTEGER NOT NULL); INSERT INTO values_under_test VALUES (1), (2), (3); SELECT count(*), sum(value) FROM values_under_test;'
run_program bash /bin/bash -c 'for i in 1 2 3; do echo "$i"; done'
run_program gcc /usr/bin/gcc --version
run_program g++ /usr/bin/g++ --version
run_program make /usr/bin/make --version
run_program bzip2 /bin/bash -c 'bzip2 -c README.md | sha256sum'
run_program gzip /bin/bash -c 'gzip -cn README.md | sha256sum'
run_program xz /bin/bash -c 'xz -c README.md | sha256sum'
run_program zstd /bin/bash -c 'zstd -q -c README.md | sha256sum'

{
    printf 'rows=%s\n' "$rows"
    printf 'passes=%s\n' "$passes"
    printf 'failures=%s\n' "$((rows - passes))"
} >>"$metadata"

printf 'Matrix complete: %s/%s backend/program rows passed.\n' "$passes" "$rows"
printf 'Results: %s\nArtifacts: %s\n' "$results" "$artifact_root"
[[ $passes -eq $rows ]]
