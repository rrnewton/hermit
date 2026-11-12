#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
cargo run -q -p hermit-manifest-plan --bin generate-validation-dag -- --write
cargo run -q -p hermit-manifest-plan --bin generate-test-footprints -- --write 2>/dev/null || cargo run -q -p hermit-manifest-plan --bin generate-test-footprints -- --check
git add ci/dag/validate.json ci/test-footprints.json 2>/dev/null || git add ci/dag/validate.json
echo "DAG regenerated"
