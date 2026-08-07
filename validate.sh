#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# THIS FILE IS A SHIM, NOT AN IMPLEMENTATION. The validation driver is
# scripts/validate.rs; there is no second version, so the two cannot drift.
#
# The shim exists only so that `validate.sh` stays a valid entrypoint NAME at
# every commit. That is what lets `git bisect`, `ci-hub validate-run`, and
# historical replay invoke ONE command across the refactor boundary, and it is
# what keeps every in-tree caller working untouched -- notably
# ci/dag/portable.json's `test.strict_compat` node, which re-enters
# `./validate.sh --portable-strict-compat-only`, plus
# .github/workflows/validation-levels.yml and scripts/test_validate_stop_paths.py.
# The Rust CLI accepts validate.sh's entire former flag surface (verified flag by
# flag), so forwarding "$@" untouched is a pure pass-through.
#
# `exec` is load-bearing: the driver must BE this process, so its pid is the one a
# caller signals, waits on, and finds in the re-entrancy marker's ancestry. The
# launcher execs the compiled binary in turn, so the chain preserves this pid.
#
# The driver is COMPILED, never dispatched through its `#!/usr/bin/env
# rust-script` shebang: GitHub's portable images ship rustc and cargo but
# intentionally not rust-script, so a shebang launch dies at execve with exit 127
# in ~0.1s having run nothing -- a no-result that reads as a test failure. See
# ci/run-rust-script.sh.
ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
exec "$ROOT_DIR/ci/run-rust-script.sh" "$ROOT_DIR/scripts/validate.rs" "$@"
