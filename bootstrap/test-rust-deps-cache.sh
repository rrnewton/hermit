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
#
# Stale and partial entries: a run whose Reindeer rewrites Cargo.lock fails and
# stores nothing, so the original lock vendors again; a keyed input that changes
# while Reindeer runs stores nothing; a fixup Git ignores, a fixup reached
# through a directory link (its referent edited), a workspace member manifest
# Git ignores, and the compiler's target cfg and version each change the key, and
# RUSTC names the compiler queried; reindeer.toml settings the key cannot cover (fixups_dir,
# rustc, cargo, gitignore_checksum_exclude, no platform table, a target in a
# multi-line or escaped string) vendor without storing, while a multi-line string
# elsewhere and Hermit's own reindeer.toml keep the cache; an eviction whose removal fails part-way leaves no entry a restore
# accepts; and an entry without its raw BUCK is replaced by the next publish.
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
  # A workspace member, when the fixture has one, as `cargo metadata` names it.
  metadata)
    if [[ -f member/Cargo.toml ]]; then
      printf '{"packages":[{"name":"member","manifest_path":"%s/member/Cargo.toml"}]}\n' "$PWD"
    else
      echo '{"packages":[]}'
    fi
    ;;
  *) echo "fixture cargo: unexpected $*" >&2; exit 2 ;;
esac
EOF
cat >"$tmp/bin/rustc" <<'EOF'
#!/usr/bin/env bash
case $1 in
  -vV) echo "${FIXTURE_RUSTC_VERSION:-rustc 1.0.0 (fixture)}" ;;
  --print=cfg)
    echo 'debug_assertions'
    echo "target_arch=\"${3%%-*}\""
    [[ -z ${FIXTURE_RUSTC_CFG:-} ]] || echo "$FIXTURE_RUSTC_CFG"
    ;;
  *) echo "fixture rustc: unexpected $*" >&2; exit 2 ;;
esac
EOF
# A RUSTC wrapper that adds a target feature, as a -C target-feature flag would.
cat >"$tmp/rustc-wrapper" <<EOF
#!/usr/bin/env bash
FIXTURE_RUSTC_CFG='target_feature="avx2"' exec "$tmp/bin/rustc" "\$@"
EOF
# An rm that, given a path in the cache, deletes one vendored file and fails,
# as an interrupted or failing recursive removal leaves an entry.
mkdir -p "$tmp/rmbin"
cat >"$tmp/rmbin/rm" <<EOF
#!/usr/bin/env bash
for arg; do
  case \$arg in
    "$tmp/cache/rust-deps/"*)
      victim=\$(/usr/bin/find "\$arg" -type f -name source -print -quit 2>/dev/null)
      [[ -z \$victim ]] || "$(type -P rm)" -f -- "\$victim"
      echo "fixture rm: failed part-way through \$arg" >&2
      exit 1
      ;;
  esac
done
exec "$(type -P rm)" "\$@"
EOF
# Reindeer stub: --third-party-dir DIR --manifest-path PATH vendor|buckify [--stdout].
# Its BUCK names the lock, the fixups it reads through links, and the target cfg
# the compiler reports. FIXTURE_REWRITE_LOCK=REV makes vendor resolve the lock
# to REV, as pinned Reindeer may (it vendors with locked: false);
# FIXTURE_EDIT_FIXUP=1 makes it edit a fixup while it runs.
cat >"$repo/bootstrap/reindeer" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
dir=$2
echo "$5" >>"$FIXTURE_REINDEER_LOG"
if [[ $5 == vendor && -n ${FIXTURE_REWRITE_LOCK:-} ]]; then
  sed -i -E "s/rev=[0-9a-f]{40}#[0-9a-f]{40}/rev=$FIXTURE_REWRITE_LOCK#$FIXTURE_REWRITE_LOCK/" Cargo.lock
fi
if [[ $5 == vendor && -n ${FIXTURE_EDIT_FIXUP:-} ]]; then
  echo '# edited while vendoring' >>"$dir/fixups/one/fixups.toml"
fi
lock=$(sha256sum <Cargo.lock | cut -c1-64)
fixups=$(find -L "$dir/fixups" -type f -print0 | LC_ALL=C sort -z | xargs -0 cat | sha256sum | cut -c1-64)
cfg=$("${RUSTC-rustc}" --print=cfg --target x86_64-unknown-linux-gnu | grep '^target_' | sha256sum | cut -c1-64)
case $5 in
  vendor)
    mkdir -p "$dir/vendor/crate" "$dir/.cargo/registry/cache"
    echo "$lock" >"$dir/vendor/crate/source"
    echo download >"$dir/.cargo/registry/cache/crate.crate"
    printf '[source.vendored-sources]\ndirectory = "vendor"\n' >"$dir/.cargo/config.toml"
    ;;
  buckify)
    raw="# raw BUCK for $lock; fixups $fixups; cfg $cfg"
    if [[ ${6:-} == --stdout ]]; then echo "$raw"
    else echo "$raw" >"$dir/BUCK"; fi
    ;;
esac
EOF
printf '#!/usr/bin/env bash\n' >"$repo/scripts/check-buck-reindeer-features.rs"
cat >"$repo/scripts/patch-reverie-dbt-buck.rs" <<'EOF'
#!/usr/bin/env bash
echo "# patched" >>"$1"
EOF
chmod +x "$tmp/bin/cargo" "$tmp/bin/rustc" "$tmp/rustc-wrapper" "$tmp/rmbin/rm" \
  "$repo/bootstrap/reindeer" "$repo/scripts/"*.rs
echo '# none' >"$repo/shim/third-party/rust/shared-cell-aliases.txt"
reindeer_toml=$repo/shim/third-party/rust/reindeer.toml
printf '[cargo]\n\n[platform.linux-x86_64]\ntarget = "x86_64-unknown-linux-gnu"\n' >"$reindeer_toml"
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
# entries [CACHE] counts the entries under a key in CACHE (default the shared one).
entries() {
  local root=${1:-$cache}/rust-deps
  [[ -d $root ]] || { echo 0; return; }
  find "$root" -mindepth 1 -maxdepth 1 -type d -regextype posix-extended \
    -regex '.*/[0-9a-f]{64}' | wc -l
}
expect_rc() {
  [[ $(cat "$tmp/$1.rc") == "$2" ]] || fail "$1: expected exit $2, got $(cat "$tmp/$1.rc"): $(cat "$tmp/$1.err")"
}
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
lock_a_hash=$(sha256sum <"$repo/Cargo.lock" | cut -c1-64)

# A Reindeer that rewrites Cargo.lock fails the run and stores nothing, so the
# original lock vendors again rather than restoring the rewritten lock's output.
regen lock_rewrite HERMIT_BUCK2_SHARED_TOOL_CACHE="$tmp/cache-lock" FIXTURE_REWRITE_LOCK="$rev_b"
expect_rc lock_rewrite 1
[[ $(entries "$tmp/cache-lock") == 0 ]] || fail "lock_rewrite: stored $(entries "$tmp/cache-lock") entries"
fixture_git -C "$repo" checkout -q -- Cargo.lock
regen lock_retry HERMIT_BUCK2_SHARED_TOOL_CACHE="$tmp/cache-lock"
expect_vendored lock_retry
grep -q "raw BUCK for $lock_a_hash" "$buck" || fail "lock_retry: BUCK is not the original lock's"

# A keyed input that changes while Reindeer runs: the output is kept, not stored.
regen input_moved HERMIT_BUCK2_SHARED_TOOL_CACHE="$tmp/cache-moved" FIXTURE_EDIT_FIXUP=1
expect_vendored input_moved
grep -q 'inputs changed while Reindeer ran' "$tmp/input_moved.err" || fail "input_moved: no warning"
[[ $(entries "$tmp/cache-moved") == 0 ]] || fail "input_moved: stored $(entries "$tmp/cache-moved") entries"
fixture_git -C "$repo" checkout -q -- shim/third-party/rust/fixups/one/fixups.toml

# A new fixup Git ignores through .git/info/exclude; Reindeer reads it anyway.
fixups=$repo/shim/third-party/rust/fixups
echo '/shim/third-party/rust/fixups/excluded/' >>"$repo/.git/info/exclude"
mkdir -p "$fixups/excluded"
echo 'cfgs = ["one"]' >"$fixups/excluded/fixups.toml"
[[ -z $(fixture_git -C "$repo" ls-files --others --exclude-standard -- shim/third-party/rust/fixups/excluded) ]] ||
  fail "excluded_fixup: the fixture's fixup is not excluded"
regen excluded_fixup
expect_vendored excluded_fixup
regen excluded_again
expect_restored excluded_again
echo 'cfgs = ["two"]' >"$fixups/excluded/fixups.toml"
regen excluded_edit
expect_vendored excluded_edit
rm -rf -- "${fixups:?}/excluded"

# A fixup reached through a directory link, then its referent edited.
mkdir -p "$tmp/outside-fixup"
echo 'cfgs = ["one"]' >"$tmp/outside-fixup/fixups.toml"
ln -s -- "$tmp/outside-fixup" "$fixups/linked"
regen link_new
expect_vendored link_new
regen link_again
expect_restored link_again
echo 'cfgs = ["two"]' >"$tmp/outside-fixup/fixups.toml"
regen link_edit
expect_vendored link_edit
rm -f -- "$fixups/linked"

# A workspace member whose manifest Git ignores, then edited.
echo '/member/' >>"$repo/.git/info/exclude"
mkdir -p "$repo/member"
printf '[package]\nname = "member"\n' >"$repo/member/Cargo.toml"
regen member_new
expect_vendored member_new
regen member_again
expect_restored member_again
echo 'edition = "2021"' >>"$repo/member/Cargo.toml"
regen member_edit
expect_vendored member_edit
rm -rf -- "${repo:?}/member"

# The compiler Reindeer queries, with the cargo version unchanged: another
# target cfg, the same again, and another version. A RUSTC wrapper that reports
# the same cfg restores that entry; with RUSTC not followed it would restore the
# plain compiler's.
regen rustc_cfg FIXTURE_RUSTC_CFG='target_feature="avx2"'
expect_vendored rustc_cfg
cmp -s "$tmp/first.tree" "$tmp/rustc_cfg.tree" && fail "rustc_cfg: BUCK did not change with the target cfg"
regen rustc_cfg_again FIXTURE_RUSTC_CFG='target_feature="avx2"'
expect_restored rustc_cfg_again
regen rustc_version FIXTURE_RUSTC_VERSION="rustc 1.0.1 (fixture)"
expect_vendored rustc_version
regen rustc_wrapper RUSTC="$tmp/rustc-wrapper"
expect_restored rustc_wrapper
cmp -s "$tmp/rustc_cfg.tree" "$tmp/rustc_wrapper.tree" || fail "rustc_wrapper: tree differs from the same cfg's"

# Settings the key cannot cover vendor, warn, and store nothing.
before=$(entries)
for setting in fixups_dir rustc cargo gitignore_checksum_exclude; do
  { echo "$setting = \"elsewhere\""; fixture_git -C "$repo" show HEAD:shim/third-party/rust/reindeer.toml; } >"$reindeer_toml"
  regen "declined_$setting"
  expect_vendored "declined_$setting"
  grep -q "not using the rust-deps cache: .*sets a path" "$tmp/declined_$setting.err" ||
    fail "declined_$setting: no warning"
done
echo '[cargo]' >"$reindeer_toml"
regen declined_no_platform
expect_vendored declined_no_platform
grep -q 'not using the rust-deps cache: .*no platform table' "$tmp/declined_no_platform.err" ||
  fail "declined_no_platform: no warning"
for value in '"""x86_64-unknown-linux-musl"""' "'''x86_64-unknown-linux-musl'''" '"x86_64\u002dunknown-linux-musl"'; do
  { fixture_git -C "$repo" show HEAD:shim/third-party/rust/reindeer.toml
    printf '\n[platform.musl]\ntarget = %s\n' "$value"; } >"$reindeer_toml"
  regen declined_quoting
  expect_vendored declined_quoting
  grep -q 'not using the rust-deps cache: .*quotes a value' "$tmp/declined_quoting.err" ||
    fail "declined_quoting: no warning for target = $value"
done
[[ $(entries) == "$before" ]] || fail "declined: cache changed from $before to $(entries) entries"
fixture_git -C "$repo" checkout -q -- shim/third-party/rust/reindeer.toml

# A multi-line string elsewhere, as Hermit's buckfile_imports is, keeps the
# cache; and Hermit's own reindeer.toml is one the cache accepts.
{ fixture_git -C "$repo" show HEAD:shim/third-party/rust/reindeer.toml
  printf '\n[buck]\nbuckfile_imports = """\nload("@prelude//rust:cargo_package.bzl", "cargo")\n"""\n'; } >"$reindeer_toml"
regen multiline_new
expect_vendored multiline_new
grep -q 'not using the rust-deps cache' "$tmp/multiline_new.err" && fail "multiline_new: cache declined"
regen multiline_again
expect_restored multiline_again
fixture_git -C "$repo" checkout -q -- shim/third-party/rust/reindeer.toml
# shellcheck source=bootstrap/rust-deps-cache.sh
if reason=$(cd -- "$script_dir/.." && source ./bootstrap/rust-deps-cache.sh && rust_deps_cache_unsupported); then
  fail "the cache declines Hermit's reindeer.toml: $reason"
fi

# An eviction whose removal fails part-way: no restore may serve what is left.
regen evict_setup
expect_restored evict_setup
regen evict_fail PATH="$tmp/rmbin:$tmp/bin:$PATH" HERMIT_RUST_DEPS_CACHE_KEEP=1 \
  FIXTURE_CARGO_VERSION="cargo 1.0.3 (fixture)"
expect_vendored evict_fail
grep -q 'fixture rm: failed part-way' "$tmp/evict_fail.err" || fail "evict_fail: the failing rm never ran"
grep -q 'could not remove the retired entry' "$tmp/evict_fail.err" || fail "evict_fail: the failed removal was not reported"
[[ $(entries) == 1 ]] || fail "evict_fail: expected 1 cache entry, found $(entries)"
regen evict_after
expect_vendored evict_after
cmp -s "$tmp/first.tree" "$tmp/evict_after.tree" || fail "evict_after: tree differs from the first run's"
leftover=$(find "$cache/rust-deps" -mindepth 1 -maxdepth 1 -name '.evict-*' | wc -l)
[[ $leftover == 0 ]] || fail "evict_after: $leftover retired entries were not removed"

# An entry without its raw BUCK is replaced by the next publish.
regen marker_setup
expect_restored marker_setup
marker_key=$(sed -n 's/^restored .* from .*\/\([0-9a-f]\{64\}\)$/\1/p' "$tmp/marker_setup.err")
rm -f -- "$cache/rust-deps/${marker_key:?}/BUCK.reindeer"
regen markerless
expect_vendored markerless
regen marker_replaced
expect_restored marker_replaced

if [[ $failures -ne 0 ]]; then
  echo "test-rust-deps-cache: $failures failure(s)" >&2
  exit 1
fi
echo "test-rust-deps-cache: ok"
