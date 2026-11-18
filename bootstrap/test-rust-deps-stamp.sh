#!/usr/bin/env bash
# Tests bootstrap/rust-deps-stamp.py in a scratch Git repository: a written stamp
# checks clean; a changed Cargo.lock, an added Cargo.toml, a changed fixup, a moved
# submodule pin and a missing stamp are each refused and named; an unrelated file
# is not an input. Run by hand: bootstrap/test-rust-deps-stamp.sh
set -euo pipefail
stamp=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/rust-deps-stamp.py
repo=$(mktemp -d)
trap 'rm -rf -- "$repo"' EXIT
git -C "$repo" init -q
mkdir -p "$repo/shim/third-party/rust/fixups/foo" "$repo/bootstrap" "$repo/crate"
echo 'lock 1' >"$repo/Cargo.lock"
echo '[workspace]' >"$repo/Cargo.toml"
echo '[package]' >"$repo/crate/Cargo.toml"
echo 'vendor = true' >"$repo/shim/third-party/rust/reindeer.toml"
echo '[[buildscript]]' >"$repo/shim/third-party/rust/fixups/foo/fixups.toml"
echo 'generator' >"$repo/bootstrap/regenerate-rust-deps"
echo 'readme' >"$repo/README.md"
git -C "$repo" add -A
git -C "$repo" update-index --add --cacheinfo 160000,1111111111111111111111111111111111111111,reverie
pass=0
expect() { # $1: 0 or 1, $2: what, $3: a phrase the refusal must name ("" for none)
    local status=0 out
    out=$("$stamp" check "$repo" 2>&1) || status=$?
    if [[ $status != "$1" ]] || [[ -n $3 && $out != *"$3"* ]]; then
        echo "FAIL: $2: exit $status, wanted $1 naming '$3': $out" >&2
        exit 1
    fi
    pass=$((pass + 1))
}
expect 1 "no stamp yet" "is missing"
"$stamp" write "$repo" >/dev/null
expect 0 "a fresh stamp" ""
grep -q '^gitlink 1111111111111111111111111111111111111111  reverie$' "$repo/shim/third-party/rust/BUCK.inputs"
echo 'readme 2' >"$repo/README.md"
expect 0 "an unrelated file changed" ""
echo 'lock 2' >"$repo/Cargo.lock"
expect 1 "Cargo.lock changed" "changed: Cargo.lock"
echo 'lock 1' >"$repo/Cargo.lock"
expect 0 "Cargo.lock restored" ""
mkdir -p "$repo/other" && echo '[package]' >"$repo/other/Cargo.toml" && git -C "$repo" add other/Cargo.toml
expect 1 "a Cargo.toml added" "added: other/Cargo.toml"
git -C "$repo" rm -q --cached other/Cargo.toml
echo 'changed' >>"$repo/shim/third-party/rust/fixups/foo/fixups.toml"
expect 1 "a fixup changed" "changed: shim/third-party/rust/fixups/foo/fixups.toml"
git -C "$repo" checkout -q -- shim/third-party/rust/fixups/foo/fixups.toml
git -C "$repo" update-index --cacheinfo 160000,2222222222222222222222222222222222222222,reverie
expect 1 "the Reverie pin moved" "changed: reverie"
git -C "$repo" update-index --cacheinfo 160000,1111111111111111111111111111111111111111,reverie
expect 0 "the pin restored" ""
echo "test-rust-deps-stamp.sh: $pass checks passed"
