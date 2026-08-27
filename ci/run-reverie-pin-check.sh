#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Compile the canonical Reverie ancestor-and-monotonic checker with the installed Rust
# toolchain instead of relying on its developer-friendly rust-script shebang.
# GitHub's portable images provide rustc but intentionally do not install
# rust-script.  Keep the wrapper dependency-free so every CI DAG lane can run
# the same checker source on a pristine image.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

mode='run'
if [[ ${1:-} == --self-test ]]; then
    mode='test'
    shift
fi

mkdir -p target/ci
compile_dir=$(mktemp -d "$ROOT_DIR/target/ci/check-reverie-pin.XXXXXX")
trap 'rm -rf -- "$compile_dir"' EXIT

# The DBT budget wrapper's self-test invokes this front door eight times.  Each
# invocation used to compile these same two dependency-free checkers into a new
# temporary directory, so one manifest audit performed sixteen identical rustc
# builds.  Retain direct rustc compilation for pristine CI images, but share the
# resulting executables when the compiler, mode, and Rust sources are identical.
#
# Hash every Rust source under scripts/, not only today's explicit #[path]
# module.  That deliberately over-invalidates when an unrelated checker changes
# so a future included module cannot be omitted from the freshness boundary.
# BEGIN CHECKER COMPILE INPUTS
compile_cached() {
    local name=$1
    local cached="$cache_dir/$name"
    if [[ ! -x $cached ]]; then
        local built="$compile_dir/$name"
        case $name in
            checker)
                if [[ $mode == test ]]; then
                    RUSTUP_TOOLCHAIN=stable rustc --edition=2021 --test \
                        scripts/check-reverie-pin.rs -o "$built" || return $?
                else
                    RUSTUP_TOOLCHAIN=stable rustc --edition=2021 \
                        scripts/check-reverie-pin.rs -o "$built" || return $?
                fi
                ;;
            uniformity)
                RUSTUP_TOOLCHAIN=stable rustc --edition=2021 \
                    scripts/check-git-pin-uniformity.rs -o "$built" || return $?
                ;;
            *)
                echo "run-reverie-pin-check: unknown checker cache entry $name" >&2
                return 2
                ;;
        esac
        mkdir -p "$cache_dir" || return $?
        # Compile outside the shared name and publish with rename. Concurrent
        # identical builders may both do the work, but neither can expose a
        # partially written executable and either complete result is valid.
        local staged="$cache_dir/.$name.$$.tmp"
        mv -- "$built" "$staged" || return $?
        mv -f -- "$staged" "$cached" || return $?
    fi
    printf '%s\n' "$cached"
}
# END CHECKER COMPILE INPUTS

compiler_and_source_key=$(
    {
        printf 'check-reverie-pin-cache-schema=1\nmode=%s\n' "$mode"
        RUSTUP_TOOLCHAIN=stable rustc -vV
        # Include the actual compile function so changing a rustc flag also
        # invalidates old output. The DBT test appends fixture behavior after
        # this function, which correctly leaves the compiled source reusable.
        sed -n '/^# BEGIN CHECKER COMPILE INPUTS$/,/^# END CHECKER COMPILE INPUTS$/p' \
            "$ROOT_DIR/ci/run-reverie-pin-check.sh"
        # The DBT wrapper test uses a symlink farm, so follow its scripts link;
        # otherwise that real invocation would hash zero source files.
        find -L scripts -type f -name '*.rs' -print0 \
            | sort -z \
            | xargs -0 sha256sum
    } | sha256sum
)
compiler_and_source_key=${compiler_and_source_key%% *}
cache_dir="$ROOT_DIR/target/ci/check-reverie-pin-cache/$compiler_and_source_key"

if [[ $mode == test ]]; then
    if (($# != 0)); then
        echo "usage: ci/run-reverie-pin-check.sh --self-test" >&2
        exit 2
    fi
    checker=$(compile_cached checker)
else
    checker=$(compile_cached checker)
fi

"$checker" "$@"

# The Reverie checker above enforces uniformity for REVERIE ONLY. Hermit pins
# three git dependencies, and until this ran, two of them -- liteinst2 and
# rust-shed -- could have been split at two revisions with nothing detecting it.
# A split pin lets a mechanism be half present: the build succeeds, the tests
# pass, and the half that actually runs is the wrong one.
#
# Run in the SAME preflight node rather than as a new one, because
# scripts/validate.rs asserts the preflight node set by exact tag and a new node
# would change that contract for a check that shares this one's subject.
#
# Compiled the same dependency-free way, for the same reason: CI images provide
# rustc but not rust-script.
if [[ $mode == run ]]; then
    uniformity=$(compile_cached uniformity)
    # STDOUT OF THIS SCRIPT IS MACHINE-READABLE: `--print-pin` callers capture it
    # with $(...) and compare the result to a 40-hex sha. The uniformity check
    # reports with `println!`, so running it on stdout appended eight lines of
    # scope commentary to that value and made every such comparison
    # unsatisfiable -- the pin was never the string being compared. Its findings
    # belong on stderr, where they are still shown and still gate via the exit
    # status, without corrupting a value another program parses.
    "$uniformity" >&2
fi
