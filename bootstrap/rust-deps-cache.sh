# shellcheck shell=bash
# Sourced by bootstrap/regenerate-rust-deps. Keeps what Reindeer generates (the
# vendored crates, the raw BUCK before postprocessing, and the vendored-sources
# Cargo configuration) in a cache entry named by everything Reindeer reads, so a
# checkout whose inputs match an earlier run's restores them instead of
# downloading and vendoring every crate again. A validation checkout is new for
# every run, so only a cache outside the checkout can serve it.
#
# The key covers Cargo.lock (which pins every crate, git sources by commit),
# every Cargo.toml and the toolchain file, the cargo version, everything under
# shim/third-party/rust that Git does not ignore (reindeer.toml, fixups), the
# ignore files Reindeer reads on the way to it, the Reindeer pin and build
# recipe, and these scripts. The postprocessing (patch-reverie-dbt-buck.rs, the
# shared-cell aliases) reads the reverie submodule, so it is not cached: it runs
# on the restored BUCK as on a generated one. HERMIT_GIT_DEP_MIRRORS is not in
# the key; a mirror changes where the pinned objects come from, never which.
#
# The root is resolved as bootstrap/run-pinned-tool resolves its cache, with
# rust-deps/ beneath it, so a validation service's HERMIT_BUCK2_SHARED_TOOL_CACHE
# serves every run on the host. HERMIT_RUST_DEPS_CACHE=off disables it. Like the
# tool cache, it trusts every checkout that writes to it: a hit is not
# re-verified, but an entry a restore cannot copy is discarded and generated
# again. Entries are about 370 MB; the newest
# HERMIT_RUST_DEPS_CACHE_KEEP (default 3) are kept, the rest are removed when an
# entry is published.
#
# These functions run where the caller's `set -e` is suspended (in conditions
# and `||` lists), so every step checks its own status.

rust_deps_cache_recipe=1

# rust_deps_cache_root prints the cache directory, or nothing when it is disabled.
rust_deps_cache_root() {
  [[ ${HERMIT_RUST_DEPS_CACHE:-} != off ]] || return 0
  local base
  if [[ -n ${HERMIT_BUCK2_SHARED_TOOL_CACHE:-} ]]; then
    base=$HERMIT_BUCK2_SHARED_TOOL_CACHE
  elif [[ -n ${HERMIT_BUCK2_TOOL_CACHE:-} ]]; then
    base=$HERMIT_BUCK2_TOOL_CACHE
  elif [[ -n ${XDG_CACHE_HOME:-} ]]; then
    base=$XDG_CACHE_HOME/hermit-buck2-tools
  else
    base=$HOME/.cache/hermit-buck2-tools
  fi
  echo "$base/rust-deps"
}

# rust_deps_cache_key prints the key of the checkout in the current directory,
# or fails without printing one.
rust_deps_cache_key() {
  local cargo_version listing path line manifest
  local -a files
  cargo_version=$(cargo --version) || return 1
  # Tracked and untracked-but-not-ignored files, so an uncommitted edit or a new
  # fixup changes the key; generated output is ignored and never in it.
  listing=$(git ls-files -z --cached --others --exclude-standard -- \
    Cargo.lock Cargo.toml ':(glob)**/Cargo.toml' rust-toolchain.toml .cargo \
    .gitignore shim/.gitignore shim/third-party/.gitignore shim/third-party/rust \
    bootstrap/regenerate-rust-deps bootstrap/rust-deps-cache.sh \
    bootstrap/reindeer bootstrap/run-pinned-tool bootstrap/tool-pins.sh |
    LC_ALL=C sort -zu | tr '\0' '\n') || return 1
  mapfile -t files <<<"$listing"
  manifest="recipe $rust_deps_cache_recipe"$'\n'"$cargo_version"$'\n'
  for path in "${files[@]}"; do
    [[ -n $path ]] || continue
    if [[ -f $path ]]; then
      line=$(sha256sum -- "$path") || return 1
    else
      # Listed by the index but deleted from the work tree.
      line="absent $path"
    fi
    manifest+=$line$'\n'
  done
  [[ $manifest == *Cargo.lock* ]] || { echo "rust-deps cache: Cargo.lock is not among the key's inputs" >&2; return 1; }
  line=$(sha256sum <<<"$manifest") || return 1
  echo "${line%% *}"
}

# rust_deps_cache_restore ROOT KEY THIRD_PARTY_DIR RAW_BUCK copies a cached entry
# into THIRD_PARTY_DIR (vendor/, .cargo/config.toml) and its raw BUCK to RAW_BUCK.
# Returns 0 on a hit, 1 when there is no entry (nothing copied), 2 when a copy
# failed (the output may be partial).
rust_deps_cache_restore() {
  local root=$1 key=$2 third_party_dir=$3 raw_buck=$4
  local entry=$root/$key
  [[ -d $root ]] || return 1
  (
    # Shared: a publish's eviction waits until no restore is copying.
    exec 9>>"$root/.lock" || exit 2
    flock -s 9 || exit 2
    [[ -f $entry/BUCK.reindeer ]] || exit 1
    touch -- "$entry" || exit 2
    mkdir -p -- "$third_party_dir/.cargo" || exit 2
    cp -a --reflink=auto -- "$entry/vendor" "$third_party_dir/vendor" || exit 2
    cp -- "$entry/cargo-config.toml" "$third_party_dir/.cargo/config.toml" || exit 2
    cp -- "$entry/BUCK.reindeer" "$raw_buck" || exit 2
  )
}

# rust_deps_cache_discard ROOT KEY removes an entry a restore could not copy, so
# this run's publish replaces it. A failure leaves it to fail, and be
# regenerated around, again.
rust_deps_cache_discard() {
  local root=$1 key=$2
  (
    exec 9>>"$root/.lock" || exit 1
    flock 9 || exit 1
    rm -rf -- "${root:?}/${key:?}"
  ) || echo "warning: could not remove $root/$key" >&2
  return 0
}

# rust_deps_cache_publish ROOT KEY THIRD_PARTY_DIR RAW_BUCK stores a generated
# entry, then removes all but the newest HERMIT_RUST_DEPS_CACHE_KEEP. A failure
# here costs later runs their hit, never this run its output, so it warns and
# returns 0.
rust_deps_cache_publish() {
  local root=$1 key=$2 third_party_dir=$3 raw_buck=$4
  local keep=${HERMIT_RUST_DEPS_CACHE_KEEP:-3}
  if [[ ! $keep =~ ^[1-9][0-9]*$ ]]; then
    echo "warning: HERMIT_RUST_DEPS_CACHE_KEEP=$keep is not a positive integer; not caching" >&2
    return 0
  fi
  (
    mkdir -p -- "$root" || exit 1
    staging=$(mktemp -d "$root/.staging-$key-XXXXXX") || exit 1
    trap 'rm -rf -- "${staging:?}"' EXIT
    cp -a --reflink=auto -- "$third_party_dir/vendor" "$staging/vendor" || exit 1
    cp -- "$third_party_dir/.cargo/config.toml" "$staging/cargo-config.toml" || exit 1
    # Written last: restore treats an entry without it as absent.
    cp -- "$raw_buck" "$staging/BUCK.reindeer" || exit 1
    # mktemp creates both private; the entry is shared like the tool cache.
    chmod 0644 -- "$staging/BUCK.reindeer" || exit 1
    chmod 0755 -- "$staging" || exit 1
    exec 9>>"$root/.lock" || exit 1
    flock 9 || exit 1
    if [[ ! -e $root/$key ]]; then
      mv -T -- "$staging" "$root/$key" || exit 1
    fi
    # Newest first by modification time; restore touches the entry it uses.
    # Staging left by an interrupted publish is removed once it is two hours old.
    find "$root" -mindepth 1 -maxdepth 1 -type d -regextype posix-extended \
      -regex '.*/[0-9a-f]{64}' -printf '%T@ %p\n' |
      sort -rn | tail -n +"$((keep + 1))" | cut -d' ' -f2- |
      while IFS= read -r old; do
        rm -rf -- "${old:?}"
      done
    find "$root" -mindepth 1 -maxdepth 1 -type d -name '.staging-*' -mmin +120 \
      -exec rm -rf -- {} +
    true
  ) || echo "warning: could not store the generated dependencies in $root; later runs will vendor again" >&2
  return 0
}
