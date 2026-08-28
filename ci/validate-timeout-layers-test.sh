#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Live, on-demand bracket for validation wall-time limits. This is not a
# default portable test: it deliberately requires a working systemd --user
# manager and cgroup-v2 delegation so a configuration-only pass is impossible.
#
# The portable strict matrix is now flat: each compatibility probe carries its
# own dagrun step timeout instead of sitting inside a second validate run. This
# script audits those deployed bounds, then proves both a named step timeout and
# the outer systemd scope timeout with live sleepers.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

runner=${DAGRUN_BIN:-$ROOT_DIR/agent-utils/rs/target/debug/dagrun}
if [[ ! -x $runner ]]; then
    cargo build --manifest-path agent-utils/rs/Cargo.toml \
        -p dagrun --bin dagrun
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/validate-timeout-layers.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT

probe_count=$(jq '[.steps[] | select(.group == "compat")] | length' ci/dag/portable.json)
probe_bounds=$(jq -r '[.steps[] | select(.group == "compat") | .timeout] | [min,max] | @tsv' \
    ci/dag/portable.json)
probe_cpu=$(jq -r '[.steps[] | select(.group == "compat") | .cpu_timeout] | unique | @tsv' \
    ci/dag/portable.json)
prep_bound=$(jq -r '.steps[] | select(.group == "compatprep" and .job == "fixtures") | [.timeout,.cpu_timeout] | @tsv' \
    ci/dag/portable.json)
nested_count=$(jq '[.steps[] | select(.cmd | contains("scripts/validate.rs --portable-strict-compat-only"))] | length' \
    ci/dag/portable.json)
if [[ $probe_count != 189 || $probe_bounds != $'20\t60' || $probe_cpu != 120 \
      || $prep_bound != $'420\t420' || $nested_count != 0 ]]; then
    printf 'validate-timeout-layers: flattened strict-compat bounds drifted: probes=%s wall=%q cpu=%q prep=%q nested=%s\n' \
        "$probe_count" "$probe_bounds" "$probe_cpu" "$prep_bound" "$nested_count" >&2
    exit 1
fi

cat >"$scratch/inner.json" <<'EOF'
{
  "description": "live inner-step timeout bracket",
  "steps": [{
    "group": "validate-timeout",
    "job": "named-inner-hang",
    "desc": "deliberate sleeper for inner step cgroup bracket",
    "cmd": "sleep 30",
    "timeout": 2,
    "cpu_timeout": 600
  }]
}
EOF

set +e
inner_output=$(
    "$runner" run --dag "$scratch/inner.json" -j 1 --run-timeout 10 \
        --no-profile-feedback --perf-dir "$scratch/inner-perf" 2>&1
)
inner_status=$?
set -e
if ((inner_status == 0)); then
    printf 'validate-timeout-layers: inner sleeper unexpectedly passed\n%s\n' "$inner_output" >&2
    exit 1
fi
inner_message=$(grep -F '[validate-timeout.named-inner-hang] ✗ FAIL' <<<"$inner_output" | tail -n 1)
inner_prefix='[validate-timeout.named-inner-hang] ✗ FAIL   deliberate sleeper for inner step cgroup bracket ('
[[ $inner_message == "$inner_prefix"*'s, TIMEOUT >2s)' ]] || {
    printf 'validate-timeout-layers: inner timeout did not name its step\n%s\n' "$inner_output" >&2
    exit 1
}
[[ $inner_output == *'cgroup boxing ACTIVE (two-level cgroup-v2 scope'* ]] || {
    printf 'validate-timeout-layers: inner bracket was not cgroup boxed\n%s\n' "$inner_output" >&2
    exit 1
}
[[ $inner_output == *'outer scope run budget ENFORCED:'*'RuntimeMaxSec=70s'* ]] || {
    printf 'validate-timeout-layers: inner bracket did not read back its wider scope bound\n%s\n' \
        "$inner_output" >&2
    exit 1
}
[[ $inner_output != *'RUN TIMEOUT'* ]] || {
    printf 'validate-timeout-layers: outer run timeout fired before the inner step bound\n%s\n' \
        "$inner_output" >&2
    exit 1
}

cat >"$scratch/control.json" <<'EOF'
{
  "description": "healthy timeout control",
  "steps": [{
    "group": "validate-timeout",
    "job": "healthy-control",
    "desc": "healthy command with generous nested bounds",
    "cmd": "true",
    "timeout": 2,
    "cpu_timeout": 600
  }]
}
EOF
control_output=$(
    "$runner" run --dag "$scratch/control.json" -j 1 --run-timeout 10 \
        --no-profile-feedback --perf-dir "$scratch/control-perf" 2>&1
)
[[ $control_output == *'[validate-timeout.healthy-control] ✓ PASS'* ]] || {
    printf 'validate-timeout-layers: healthy control did not pass\n%s\n' "$control_output" >&2
    exit 1
}
[[ $control_output != *'TIMEOUT'* ]] || {
    printf 'validate-timeout-layers: a timeout fired on the healthy control\n%s\n' "$control_output" >&2
    exit 1
}

unit="hermit-validate-outer-bracket-${BASHPID}-${RANDOM}"
set +e
outer_output=$(
    systemd-run --user --wait --collect --pipe --unit="$unit" \
        -p RuntimeMaxSec=2s /bin/bash -c \
        'printf "OUTER-CGROUP-BRACKET-STARTED\n"; sleep 30; printf "OUTER-CGROUP-BRACKET-ESCAPED\n"' \
        2>&1
)
outer_status=$?
set -e
if ((outer_status == 0)); then
    printf 'validate-timeout-layers: outer cgroup sleeper unexpectedly passed\n%s\n' \
        "$outer_output" >&2
    exit 1
fi
outer_message='Finished with result: timeout'
[[ $outer_output == *'OUTER-CGROUP-BRACKET-STARTED'* && \
   $outer_output == *"$outer_message"* && \
   $outer_output == *'status=15/TERM'* && \
   $outer_output != *'OUTER-CGROUP-BRACKET-ESCAPED'* ]] || {
    printf 'validate-timeout-layers: outer RuntimeMaxSec did not fire distinctly\n%s\n' \
        "$outer_output" >&2
    exit 1
}

printf 'VALIDATE-TIMEOUT-LAYERS negative-inner=1 message=%q\n' "$inner_message"
printf 'VALIDATE-TIMEOUT-LAYERS negative-outer=1 message=%q\n' "$outer_message"
printf 'VALIDATE-TIMEOUT-LAYERS positive=1 healthy-control=PASS\n'
printf 'VALIDATE-TIMEOUT-LAYERS deployed=189 direct compat probes wall=20..60s cpu=120s prep=420s nested=0\n'
