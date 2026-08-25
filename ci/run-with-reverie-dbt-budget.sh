#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Re-derive the Reverie DBT elapsed budget inside the safe-ci child. Under
# cgroup boxing the runner exports its cap-derived CARGO_BUILD_JOBS immediately
# before this wrapper; on an unboxed hosted runner the launch-time
# CI_DAG_BUILD_JOBS value remains the fallback. Keeping this wrapper immediately
# around Cargo prevents a launcher-side width from standing in for NUM_JOBS.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if (($# == 0)); then
    echo "usage: ci/run-with-reverie-dbt-budget.sh COMMAND [ARG...]" >&2
    exit 2
fi

# Bind the calibration to the exact local Reverie revision before applying it.
# --print-pin is deliberately offline: the separate latest-main gate owns the
# network authority, while this check prevents a pin bump from silently reusing
# an earlier revision's clamp and measured threshold.
# CALIBRATED FOR f4152f8f BY DBT RECIPE IDENTITY, the same evidence this budget
# has been carried on five times before. Between 13cf8bcb and f4152f8f the two
# repository inputs are BYTE-IDENTICAL by git object id:
#     reverie-dbt/vendor/dynamorio  de352475846e -> de352475846e
#     reverie-dbt/build.rs          0ff8ae24b974 -> 0ff8ae24b974
# source_recipe_key also hashes the selected CMAKE and CMAKE_GENERATOR.
# This carry is stronger than the previous four, which each had to argue that some
# reverie-dbt Rust change was not a recipe input. Here `git diff 13cf8bcb f4152f8f
# -- reverie-dbt` is EMPTY: the directory is unchanged in its entirety, so there is
# no such argument to make, and the empirical install-key check below confirms the
# complete selected recipe.
#
# AND IT WAS CONFIRMED EMPIRICALLY AT THE NEW PIN, not only by source comparison.
# A build at f4152f8f produces the DynamoRIO install key this budget was measured
# against, in both profiles:
#     target/{debug,release}/reverie-dbt-native-cache/dynamorio-install-132d7713...
# Reproduced from a genuinely cold state -- the native cache was deleted and
# reverie-dbt `cargo clean`ed first, and the rebuild landed on the same key.
# The recipe key is the thing the measurement is a property of, so an identical key
# means the measured work is identical.
#
# WHAT IS CALIBRATED IS 1050 EFFECTIVE JOB-SECONDS, not an elapsed wall time.
# ci/configure-build-jobs.sh derives the elapsed bound as
#     MAX_BUILD_SECONDS = ceil(MAX_BUILD_EFFECTIVE_JOB_SECONDS / EFFECTIVE_BUILD_JOBS)
# so the budget already scales with width and must not be "topped up" by hand. If
# it is ever too tight, re-measure the job-seconds; do not raise the elapsed bound.
expected_pin=f4152f8fd3a6d234e9ba4946ef3f9fa27aa7f8a7

# TAKE THE PIN, NOT WHATEVER ELSE THE PRODUCER PRINTED.
#
# This captured the whole of `--print-pin` and compared it. A later change made
# that command also emit a pin-uniformity report on stdout, so the capture became
# 941 characters over 8 lines and the comparison below COULD NEVER SUCCEED FOR
# ANY PIN, including a correctly calibrated one. Every node behind this wrapper
# then failed closed in about a second, and updating `expected_pin` could not fix
# it because the recorded side was never a sha. The producer is fixed too -- its
# report now goes to stderr -- but a value parsed out of a shared stream should
# be validated by the consumer rather than trusted to stay clean.
recorded_pin=$(
    "$ROOT_DIR/ci/run-reverie-pin-check.sh" --repo "$ROOT_DIR" --print-pin
)
if [[ ! $recorded_pin =~ ^[0-9a-f]{40}$ ]]; then
    echo "run-with-reverie-dbt-budget.sh: --print-pin did not yield a 40-hex revision; got ${#recorded_pin} char(s): ${recorded_pin:0:80}" >&2
    echo "run-with-reverie-dbt-budget.sh: NOT RUNNING '$*' against an unidentified Reverie pin" >&2
    exit 2
fi
if [[ $recorded_pin != "$expected_pin" ]]; then
    echo "run-with-reverie-dbt-budget.sh: no calibrated budget for Reverie pin $recorded_pin (expected $expected_pin)" >&2
    echo "run-with-reverie-dbt-budget.sh: NOT RUNNING '$*'. To recalibrate, confirm reverie-dbt/vendor/dynamorio and reverie-dbt/build.rs are unchanged and CMAKE/CMAKE_GENERATOR select the same tooling between the pins, then update expected_pin here." >&2
    exit 2
fi
REVERIE_DBT_BUDGET_BOUND_PIN=$recorded_pin
export REVERIE_DBT_BUDGET_BOUND_PIN

# shellcheck source=ci/configure-build-jobs.sh
source "$ROOT_DIR/ci/configure-build-jobs.sh" reverie-dbt-budget-child

echo "run-with-reverie-dbt-budget.sh: reverie-dbt-budget={pin:$REVERIE_DBT_BUDGET_BOUND_PIN,source:$REVERIE_DBT_BUILD_JOBS_SOURCE,raw-build-jobs:$REVERIE_DBT_RAW_BUILD_JOBS,effective-cpus-source:$REVERIE_DBT_EFFECTIVE_CPUS_SOURCE,effective-cpus:$REVERIE_DBT_EFFECTIVE_CPUS,reverie-max-jobs:$REVERIE_DBT_MAX_PARALLEL_JOBS,effective-native-jobs:$REVERIE_DBT_EFFECTIVE_BUILD_JOBS,effective-job-seconds:$REVERIE_DBT_MAX_BUILD_EFFECTIVE_JOB_SECONDS,max-elapsed-seconds:$REVERIE_DBT_MAX_BUILD_SECONDS,basis:github-portable-cold-miss-n3-affinity4,carried-to-pin-on-dynamorio-recipe-key:132d77130980c546c8867fc196d97e664bc4816b1dfa9ea9c18de4a94d109c4d}" >&2

exec "$@"
