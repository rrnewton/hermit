#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Re-derive the Reverie DBI elapsed budget inside the safe-ci child. Under
# cgroup boxing the runner exports its cap-derived CARGO_BUILD_JOBS immediately
# before this wrapper; on an unboxed hosted runner the launch-time
# CI_DAG_BUILD_JOBS value remains the fallback. Keeping this wrapper immediately
# around Cargo prevents a launcher-side width from standing in for NUM_JOBS.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if (($# == 0)); then
    echo "usage: ci/run-with-reverie-dbi-budget.sh COMMAND [ARG...]" >&2
    exit 2
fi

# Bind the calibration to the local Reverie revision before applying it.
# --print-pin is deliberately offline: the separate latest-main gate owns the
# network authority, while this check prevents a pin bump from silently reusing
# an earlier revision's clamp and measured threshold.
#
# The binding is on the DynamoRIO recipe inputs, not on the pin: the pin is a
# proxy that changed six times while the recipe changed none. See
# ci/reverie-dbi-budget-calibration.env for the record and the derivation.
# shellcheck source=ci/reverie-dbi-budget-calibration.env
source "$ROOT_DIR/ci/reverie-dbi-budget-calibration.env"
recorded_pin=$(
    "$ROOT_DIR/ci/run-reverie-pin-check.sh" --repo "$ROOT_DIR" --print-pin
)
if ! REVERIE_DBI_BUDGET_BINDING=$(reverie_dbi_budget_pin_is_calibrated "$recorded_pin"); then
    echo "run-with-reverie-dbi-budget.sh: no calibrated budget for Reverie pin $recorded_pin (calibrated recipe key $REVERIE_DBI_CALIBRATED_RECIPE_KEY)" >&2
    exit 2
fi
REVERIE_DBI_BUDGET_BOUND_PIN=$recorded_pin
export REVERIE_DBI_BUDGET_BINDING
export REVERIE_DBI_BUDGET_BOUND_PIN

# shellcheck source=ci/configure-build-jobs.sh
source "$ROOT_DIR/ci/configure-build-jobs.sh" reverie-dbi-budget-child

echo "run-with-reverie-dbi-budget.sh: reverie-dbi-budget={pin:$REVERIE_DBI_BUDGET_BOUND_PIN,source:$REVERIE_DBI_BUILD_JOBS_SOURCE,raw-build-jobs:$REVERIE_DBI_RAW_BUILD_JOBS,effective-cpus-source:$REVERIE_DBI_EFFECTIVE_CPUS_SOURCE,effective-cpus:$REVERIE_DBI_EFFECTIVE_CPUS,reverie-max-jobs:$REVERIE_DBI_MAX_PARALLEL_JOBS,effective-native-jobs:$REVERIE_DBI_EFFECTIVE_BUILD_JOBS,effective-job-seconds:$REVERIE_DBI_MAX_BUILD_EFFECTIVE_JOB_SECONDS,max-elapsed-seconds:$REVERIE_DBI_MAX_BUILD_SECONDS,basis:$REVERIE_DBI_CALIBRATED_BASIS,binding:$REVERIE_DBI_BUDGET_BINDING,carried-to-pin-on-dynamorio-recipe-key:$REVERIE_DBI_CALIBRATED_RECIPE_KEY}" >&2

exec "$@"
