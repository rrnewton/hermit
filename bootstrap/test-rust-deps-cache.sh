#!/usr/bin/env bash
# Regression test for the cache in bootstrap/regenerate-rust-deps
# (bootstrap/rust-deps-cache.sh).
#
# Runs the real regenerate-rust-deps and cache helper in a fixture checkout
# whose cargo, Reindeer, feature check and reverie-dbt patch are stubs; the
# Reindeer stub logs each call and derives its output from Cargo.lock. Accept:
# the first run vendors and stores an entry; a second run with the same inputs
# calls Reindeer not at all and leaves a byte-identical tree, with the
# postprocessing applied once. Re-vendor: a committed Cargo.lock that moves a
# git dependency to another commit, an edited reindeer.toml, a new untracked
# fixup, and a different cargo version each vendor again, and a lock moved back
# restores its earlier entry. Off: HERMIT_RUST_DEPS_CACHE=off always vendors
# and stores nothing. Eviction keeps the newest HERMIT_RUST_DEPS_CACHE_KEEP
# entries by last use. Damage: an entry whose vendored crates are gone is
# discarded, the run vendors, and the replacement entry serves the next run.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
for f in regenerate-rust-deps rust-deps-cache.sh; do
  [[ -f $script_dir/$f ]] || { echo "test-rust-deps-cache: missing $script_dir/$f" >&2; exit 2; }
done

tmp=$(mktemp -d)
trap 'rm -rf "${tmp:?}"' EXIT
failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }

fixture_git() {
  git -c user.email=fixture@example.invalid -c user.name=fixture \
    -c commit.gpgsign=false -c init.defaultBranch=main "$@"
}

repo=$tmp/repo
cache=$tmp/cache
log=$tmp/reindeer.log
mkdir -p "$tmp/bin" "$repo/bootstrap" "$repo/scripts" "$repo/reverie" \
  "$repo/shim/third-party/rust/fixups/one"
cp -- "$script_dir/regenerate-rust-deps" "$script_dir/rust-deps-cache.sh" "$repo/bootstrap/"

cat >"$tmp/bin/cargo" <<'EOF'
#!/usr/bin/env bash
case $1 in
  --version) echo "${FIXTURE_CARGO_VERSION:-cargo 1.0.0 (fixture)}" ;;
  metadata) ;;
  *) echo "fixture cargo: unexpected $*" >&2; exit 2 ;;
esac
EOF
# Reindeer stub: --third-party-dir DIR --manifest-path PATH vendor|buckify [--stdout].
cat >"$repo/bootstrap/reindeer" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
dir=$2
echo "$5" >>"$FIXTURE_REINDEER_LOG"
lock=$(sha256sum <Cargo.lock | cut -c1-64)
case $5 in
  vendor)
    mkdir -p "$dir/vendor/crate" "$dir/.cargo/registry/cache"
    echo "$lock" >"$dir/vendor/crate/source"
    echo download >"$dir/.cargo/registry/cache/crate.crate"
    printf '[source.vendored-sources]\ndirectory = "vendor"\n' >"$dir/.cargo/config.toml"
    ;;
  buckify)
    if [[ ${6:-} == --stdout ]]; then echo "# raw BUCK for $lock"
    else echo "# raw BUCK for $lock" >"$dir/BUCK"; fi
    ;;
esac
EOF
printf '#!/usr/bin/env bash\n' >"$repo/scripts/check-buck-reindeer-features.rs"
cat >"$repo/scripts/patch-reverie-dbt-buck.rs" <<'EOF'
#!/usr/bin/env bash
echo "# patched" >>"$1"
EOF
chmod +x "$tmp/bin/cargo" "$repo/bootstrap/reindeer" "$repo/scripts/"*.rs
echo '# none' >"$repo/shim/third-party/rust/shared-cell-aliases.txt"
echo '[cargo]' >"$repo/shim/third-party/rust/reindeer.toml"
echo 'extra_srcs = []' >"$repo/shim/third-party/rust/fixups/one/fixups.toml"
echo 'buck' >"$repo/reverie/BUCK"
printf '[workspace]\nmembers = []\n' >"$repo/Cargo.toml"
printf '/shim/third-party/rust/.cargo/\n/shim/third-party/rust/BUCK\n/shim/third-party/rust/vendor/\n' \
  >"$repo/.gitignore"
lock_at() {
  cat >"$repo/Cargo.lock" <<EOF
version = 4

[[package]]
name = "dep"
version = "0.1.0"
source = "git+https://example.invalid/org/dep?rev=$1#$1"
EOF
}
commit_all() {
  fixture_git -C "$repo" add -A
  fixture_git -C "$repo" commit -qm "$1"
}
rev_a=1111111111111111111111111111111111111111
rev_b=2222222222222222222222222222222222222222
fixture_git init -q "$repo"
lock_at "$rev_a"
commit_all base

# regen NAME [ENV...]: runs regenerate-rust-deps with a private cache root that
# keeps every entry unless ENV says otherwise; records its rc, stderr, the
# Reindeer calls it made, and a hash of the tree.
regen() {
  local name=$1
  shift
  : >"$log"
  local rc=0
  env -i PATH="$tmp/bin:$PATH" HOME="$tmp" GIT_CONFIG_NOSYSTEM=1 \
    HERMIT_BUCK2_SHARED_TOOL_CACHE="$cache" HERMIT_RUST_DEPS_CACHE_KEEP=100 \
    FIXTURE_REINDEER_LOG="$log" "$@" \
    "$repo/bootstrap/regenerate-rust-deps" >"$tmp/$name.out" 2>"$tmp/$name.err" || rc=$?
  echo "$rc" >"$tmp/$name.rc"
  tr '\n' ' ' <"$log" >"$tmp/$name.calls"
  (cd "$repo/shim/third-party/rust" && find BUCK .cargo vendor -type f -print0 |
    LC_ALL=C sort -z | xargs -0 sha256sum) >"$tmp/$name.tree" 2>&1 || true
}
calls() { cat "$tmp/$1.calls"; }
entries() { find "$cache/rust-deps" -mindepth 1 -maxdepth 1 -type d -regextype posix-extended \
  -regex '.*/[0-9a-f]{64}' | wc -l; }
expect_ok() {
  [[ $(cat "$tmp/$1.rc") == 0 ]] || { fail "$1: exit $(cat "$tmp/$1.rc"): $(cat "$tmp/$1.err")"; return 0; }
}
expect_vendored() {
  expect_ok "$1"
  [[ $(calls "$1") == "vendor buckify buckify " ]] || fail "$1: expected Reindeer to vendor, called: '$(calls "$1")'"
  ! grep -q '^restored' "$tmp/$1.err" || fail "$1: reported a restore"
}
expect_restored() {
  expect_ok "$1"
  [[ -z $(calls "$1") ]] || fail "$1: expected a restore, but Reindeer ran: '$(calls "$1")'"
  grep -q '^restored the vendored crates' "$tmp/$1.err" || fail "$1: no restore reported"
}
buck=$repo/shim/third-party/rust/BUCK

# First run vendors, prunes Cargo's download caches, and stores one entry.
regen first
expect_vendored first
[[ $(entries) == 1 ]] || fail "first: expected 1 cache entry, found $(entries)"
[[ ! -e $repo/shim/third-party/rust/.cargo/registry ]] || fail "first: Cargo's download cache was kept"
[[ $(grep -c '^# patched$' "$buck") == 1 ]] || fail "first: postprocessing ran $(grep -c '^# patched$' "$buck") times"

# Same inputs: no Reindeer call, the same tree, postprocessing applied once.
regen second
expect_restored second
cmp -s "$tmp/first.tree" "$tmp/second.tree" || fail "second: restored tree differs from the generated one"
[[ $(entries) == 1 ]] || fail "second: expected 1 cache entry, found $(entries)"

# A committed Cargo.lock that moves a git dependency vendors again, for its lock.
lock_at "$rev_b"
commit_all "move dep"
regen lock_b
expect_vendored lock_b
cmp -s "$tmp/first.tree" "$tmp/lock_b.tree" && fail "lock_b: tree did not change with the lock"
grep -q "$(sha256sum <"$repo/Cargo.lock" | cut -c1-64)" "$buck" || fail "lock_b: BUCK is not the new lock's"
[[ $(entries) == 2 ]] || fail "lock_b: expected 2 cache entries, found $(entries)"

# Moving the lock back restores the first entry.
lock_at "$rev_a"
commit_all "move dep back"
regen lock_a
expect_restored lock_a
cmp -s "$tmp/first.tree" "$tmp/lock_a.tree" || fail "lock_a: tree differs from the first run's"

# An uncommitted reindeer.toml edit, a new untracked fixup, and another cargo
# version each change the key.
echo 'vendor = true' >>"$repo/shim/third-party/rust/reindeer.toml"
regen reindeer_toml
expect_vendored reindeer_toml
fixture_git -C "$repo" checkout -q -- shim/third-party/rust/reindeer.toml
mkdir -p "$repo/shim/third-party/rust/fixups/two"
echo 'extra_srcs = []' >"$repo/shim/third-party/rust/fixups/two/fixups.toml"
regen new_fixup
expect_vendored new_fixup
rm -rf -- "${repo:?}/shim/third-party/rust/fixups/two"
regen cargo_version FIXTURE_CARGO_VERSION="cargo 1.0.1 (fixture)"
expect_vendored cargo_version

# Off: vendors although an entry matches, and stores nothing.
before=$(entries)
regen off HERMIT_RUST_DEPS_CACHE=off
expect_vendored off
cmp -s "$tmp/first.tree" "$tmp/off.tree" || fail "off: tree differs from the first run's"
[[ $(entries) == "$before" ]] || fail "off: cache changed from $before to $(entries) entries"

# Eviction by last use: of the five entries, use the oldest, then publish a
# sixth with KEEP=2; those two remain.
[[ $(entries) == 5 ]] || fail "before eviction: expected 5 cache entries, found $(entries)"
regen use_base
expect_restored use_base
base_key=$(sed -n 's/^restored .* from .*\/\([0-9a-f]\{64\}\)$/\1/p' "$tmp/use_base.err")
regen keep2 HERMIT_RUST_DEPS_CACHE_KEEP=2 FIXTURE_CARGO_VERSION="cargo 1.0.2 (fixture)"
expect_vendored keep2
[[ $(entries) == 2 ]] || fail "keep2: expected 2 cache entries, found $(entries)"
[[ -n $base_key && -d $cache/rust-deps/$base_key ]] || fail "keep2: evicted the most recently used entry"

# Damage: an entry without its vendored crates is discarded and replaced.
rm -rf -- "${cache:?}/rust-deps/${base_key:?}/vendor"
regen damaged
expect_vendored damaged
grep -q 'discarding it and generating' "$tmp/damaged.err" || fail "damaged: no discard reported"
cmp -s "$tmp/first.tree" "$tmp/damaged.tree" || fail "damaged: tree differs from the first run's"
regen repaired
expect_restored repaired

if [[ $failures -ne 0 ]]; then
  echo "test-rust-deps-cache: $failures failure(s)" >&2
  exit 1
fi
echo "test-rust-deps-cache: ok"
