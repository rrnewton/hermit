#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Validate a PR issue-comment array from GitHub. A substitution is auditable
# only when validate.sh recorded the exact head, profile, zero-failure result,
# durable host/log path, timestamp, and machine-readable evidence marker.
set -euo pipefail

if (($# != 1)) || [[ ! $1 =~ ^[0-9a-fA-F]{40}$ ]]; then
    echo "usage: $0 HEAD_SHA < comments.json" >&2
    exit 2
fi

head_sha=${1,,}

evidence=$(jq -er --arg sha "$head_sha" '
    [ .[]
      | select(.body | type == "string")
      | .body as $body
      | select($body | startswith("[impl agent, validate.sh]"))
      | select($body | contains("Local validation passed"))
      | select($body | contains("- SHA: `" + $sha + "`"))
      | select($body | test("- Profile: `[^`]+`"))
      | select($body | test("- Results: [1-9][0-9]* checks passed, 0 failed"))
      | select($body | test("- Log: `[^`]+:[^`]+`"))
      | select($body | test("- Timestamp \\(UTC\\): `[^`]+`"))
      | select($body | contains("<!-- locally-validated-evidence sha=" + $sha + " "))
      | select($body | test("profile=[^ ]+ host=[^ ]+ log=[^ ]+ ts=[^ ]+ -->"))
      | .body
    ]
    | last // error("no complete exact-head local-validation evidence comment")
') || {
    echo "check-local-validation-evidence.sh: no complete evidence for $head_sha" >&2
    exit 1
}

# shellcheck disable=SC2016
profile=$(sed -n 's/^- Profile: `\([^`]*\)`.*/\1/p' <<<"$evidence" | head -n 1)
# shellcheck disable=SC2016
log_path=$(sed -n 's/^- Log: `\([^`]*\)`.*/\1/p' <<<"$evidence" | head -n 1)
echo "check-local-validation-evidence.sh: exact-head evidence found (sha=$head_sha profile=$profile log=$log_path)"
