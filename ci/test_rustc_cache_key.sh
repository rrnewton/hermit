#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Cache-key regression test for the rustc version stamp (shim/BUCK reads
# [hermit] rustc_version from .buckconfig and passes
# -Cmetadata=hermit-rustc-<version> on every rustc action).
#
# Before the stamp existed, the shared localhost action cache served an
# rlib built by a floating nightly to the pinned stable build, which
# rustc refused with E0514: no action key named the compiler.
#
# Against a PRIVATE scratch cache (never the shared one), one leaf Rust
# target is built four times, wiping buck-out between builds so only
# the cache can serve. Per build, `buck2 log what-ran` JSON is filtered
# to rustc-category actions (identity ends "(rustc ...)" - rlib,
# metadata, bin, proc-macro and build-script compiles alike):
#   1. pinned           populates; records the rustc action count N
#   2. pinned again     positive control: rustc cache hits must be > 0
#   3. version flipped  ZERO rustc cache hits, exactly N rustc actions
#                       executed locally, build green
#   4. control flip     an unrelated buckconfig key flipped instead:
#                       rustc cache hits must return (> 0), proving the
#                       harness can still see hits and build 3's zero
#                       is the stamp, not a broken cache
# On a tree without the stamp, build 3 is served like build 2 and this
# test fails; against a cache holding another compiler's rlibs that
# build is the E0514 reproducer instead. Earlier versions compared
# output paths and total cached counts: both are wrong observables
# (paths do not move with -Cmetadata, most actions are copies that
# legitimately hit, and a shared cache remembers earlier flips).
#
# Requires a bazel-remote binary ($BAZEL_REMOTE, else
# ~/.local/bin/bazel-remote-hermit) and the shared client config to
# derive the scratch one from. Backs up .buckconfig and
# .buckconfig.local BEFORE touching anything and restores them
# byte-for-byte on every exit path. Deletes buck-out four times; run in
# a disposable slot, on cache-linked team hosts and after any shim
# toolchain change. Not wired into the validate DAG yet (it runs buck
# inside the check).
#
# Usage: BUCK2=/path/to/buck2 ci/test_rustc_cache_key.sh [target]

set -euo pipefail

BUCK2="${BUCK2:-./bootstrap/buck2}"
TARGET="${1:-//detcore-model:detcore-model}"
REMOTE="${BAZEL_REMOTE:-$HOME/.local/bin/bazel-remote-hermit}"
PORT="${RUSTC_CACHE_KEY_PORT:-39391}"

scratch="$(mktemp -d "${TMPDIR:-/tmp}/rustc-cache-key.XXXXXX")"
remote_pid=""

# Backups FIRST, before the server or any possible exit path.
cp .buckconfig "$scratch/buckconfig.bak"
buckconfig_sha_before="$(sha256sum .buckconfig | cut -d" " -f1)"
had_local=0
local_target=""
if [[ -L .buckconfig.local ]]; then
    had_local=1
    local_target="$(readlink .buckconfig.local)"
elif [[ -f .buckconfig.local ]]; then
    had_local=1
    cp .buckconfig.local "$scratch/buckconfig.local.bak"
fi

cleanup() {
    cp "$scratch/buckconfig.bak" .buckconfig
    rm -f .buckconfig.local
    if [[ "$had_local" == 1 ]]; then
        if [[ -n "$local_target" ]]; then
            ln -s "$local_target" .buckconfig.local
        else
            cp "$scratch/buckconfig.local.bak" .buckconfig.local
        fi
    fi
    if [[ -n "$remote_pid" ]]; then
        kill "$remote_pid" 2>/dev/null || true
        wait "$remote_pid" 2>/dev/null || true
    fi
    if [[ "$(sha256sum .buckconfig | cut -d" " -f1)" != "$buckconfig_sha_before" ]]; then
        echo ":: [rustc-cache-key] ERROR: .buckconfig not restored byte-for-byte" >&2
        exit 3
    fi
    rm -rf "$scratch"
}
trap cleanup EXIT

if [[ ! -x "$REMOTE" ]]; then
    echo ":: [rustc-cache-key] SKIP-FAIL: no bazel-remote at $REMOTE" >&2
    exit 2
fi
if [[ ! -f "$HOME/.config/hermit/buckconfig.local" ]]; then
    echo ":: [rustc-cache-key] SKIP-FAIL: no shared cache client config" >&2
    exit 2
fi

setsid "$REMOTE" --dir "$scratch/cache" --max_size 5 \
    --http_address "127.0.0.1:$((PORT - 1))" --grpc_address "127.0.0.1:$PORT" \
    </dev/null >"$scratch/remote.log" 2>&1 &
remote_pid=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
    ss -ltn | grep -q ":$PORT " && break
    sleep 1
done
ss -ltn | grep -q ":$PORT " || {
    echo ":: [rustc-cache-key] scratch bazel-remote did not listen on $PORT" >&2
    exit 2
}
sed "s/127.0.0.1:[0-9]*/127.0.0.1:$PORT/" \
    "$HOME/.config/hermit/buckconfig.local" > .buckconfig.local.scratch
mv .buckconfig.local.scratch .buckconfig.local

# "executed cached" for rustc-category actions of the last build.
rustc_counts() {
    "$BUCK2" log what-ran --format json 2>/dev/null | python3 -c '
import json, sys, collections
rows = []
for line in sys.stdin:
    line = line.strip()
    if line.startswith("{"):
        try: rows.append(json.loads(line))
        except Exception: pass
rust = [r for r in rows if "(rustc" in r.get("identity", "")]
c = collections.Counter((r.get("reproducer") or {}).get("executor") for r in rust)
print(c.get("Cache", 0), c.get("Local", 0) + c.get("Remote", 0), len(rust))
'
}

fresh_build() {
    echo ":: [rustc-cache-key] $1" >&2
    "$BUCK2" kill >/dev/null 2>&1 || true
    rm -rf buck-out
    # errexit does not reach into $(...) captures of this function, so
    # without this check a failed build only yields empty counts and
    # could pass on a later comparison. A failed build fails the test.
    "$BUCK2" build "$TARGET" >/dev/null || {
        echo ":: [rustc-cache-key] FAIL: build failed in step: $1" >&2
        exit 1
    }
    rustc_counts
}

read -r c1hits c1exec c1total <<<"$(fresh_build 'build 1: pinned (populate scratch cache)')"
read -r c2hits c2exec c2total <<<"$(fresh_build 'build 2: pinned again (positive control)')"
echo ":: [rustc-cache-key] rustc cached/executed/total: b1=$c1hits/$c1exec/$c1total b2=$c2hits/$c2exec/$c2total"
if [[ "${c2hits:-0}" -eq 0 || "${c1total:-0}" -eq 0 ]]; then
    echo ":: [rustc-cache-key] FAIL: control builds are not cache-served;" >&2
    echo "   cannot judge action keys." >&2
    exit 1
fi

sed -i 's/^rustc_version = .*/rustc_version = 0.0.0-cachekey-redtest/' .buckconfig
read -r c3hits c3exec c3total <<<"$(fresh_build 'build 3: flipped version (expect zero rustc hits)')"
cp "$scratch/buckconfig.bak" .buckconfig

# Control: same dance, but flip a key that must NOT reach action keys.
printf '\n[hermit]\ncachekey_control = flipped\n' >> .buckconfig
read -r c4hits c4exec c4total <<<"$(fresh_build 'build 4: control flip (expect rustc hits back)')"
cp "$scratch/buckconfig.bak" .buckconfig
echo ":: [rustc-cache-key] flipped=$c3hits/$c3exec/$c3total control=$c4hits/$c4exec/$c4total"

fail=0
if [[ "${c3hits:-1}" -ne 0 ]]; then
    echo ":: [rustc-cache-key] FAIL: flipped build got $c3hits rustc cache hits, want 0" >&2
    fail=1
fi
if [[ "${c3exec:-0}" -ne "${c1exec:-1}" ]]; then
    echo ":: [rustc-cache-key] FAIL: flipped build executed ${c3exec:-?} rustc actions," >&2
    echo "   build 1 executed ${c1exec:-?}; a class of actions lost the stamp" >&2
    fail=1
fi
if [[ "${c4hits:-0}" -eq 0 ]]; then
    echo ":: [rustc-cache-key] FAIL: control flip also missed everything;" >&2
    echo "   the harness cannot distinguish a stamp from noise" >&2
    fail=1
fi
[[ "$fail" -eq 0 ]] || exit 1
echo ":: [rustc-cache-key] PASS: zero rustc hits after flip, full local recompile, control hits."
