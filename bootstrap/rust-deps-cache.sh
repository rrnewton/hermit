# shellcheck shell=bash
# Sourced by bootstrap/regenerate-rust-deps. Keeps what Reindeer generates (the
# vendored crates, the raw BUCK before postprocessing, and the vendored-sources
# Cargo configuration) in a cache entry named by everything Reindeer reads, so a
# checkout whose inputs match an earlier run's restores them instead of
# downloading and vendoring every crate again. A validation checkout is new for
# every run, so only a cache outside the checkout can serve it.
#
# The key covers Cargo.lock (which pins every crate, git sources by commit), the
# workspace manifests (every Cargo.toml Git lists and every member Cargo reports),
# the toolchain file and Cargo configuration, the cargo version, the compiler
# Reindeer queries (its `-vV` and the `target_` cfg it prints for each platform
# target in reindeer.toml), every file under shim/third-party/rust except the
# generated output, whether or not Git ignores it, read through symbolic links
# with each link's target recorded, the ignore files Reindeer reads on the way
# to it, the Reindeer pin and build recipe, and these scripts. Settings that make
# Reindeer read files or a compiler the key cannot name (fixups_dir, cargo,
# rustc, gitignore_checksum_exclude, no platform table, or quoting the target
# scan cannot read) decline the cache: the run generates and stores nothing.
# The postprocessing (patch-reverie-dbt-buck.rs, the shared-cell aliases) reads
# the reverie submodule, so it is not cached: it runs on the restored BUCK as on
# a generated one. HERMIT_GIT_DEP_MIRRORS is not in the key; a mirror changes
# where the pinned objects come from, never which. Cargo configuration outside
# the checkout is not either: it can change resolution only by rewriting
# Cargo.lock, which the caller refuses before anything is stored.
#
# The root is resolved as bootstrap/run-pinned-tool resolves its cache, with
# rust-deps/ beneath it, so a validation service's HERMIT_BUCK2_SHARED_TOOL_CACHE
# serves every run on the host. HERMIT_RUST_DEPS_CACHE=off disables it. Like the
# tool cache, it trusts every checkout that writes to it: a hit is not
# re-verified, but an entry a restore cannot copy is discarded and generated
# again. An entry is public under its key only once complete (built in a staging
# directory, renamed in) and leaves that name before anything in it is removed
# (renamed out to an .evict- directory), so an interrupted publish or eviction
# never leaves a partial entry a restore accepts. Entries are about 400 MB; the
# newest HERMIT_RUST_DEPS_CACHE_KEEP (default 3) are kept, the rest are removed
# when an entry is published.
#
# These functions run where the caller's `set -e` is suspended (in conditions
# and `||` lists), so every step checks its own status.

rust_deps_cache_recipe=2

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

# rust_deps_cache_targets REINDEER_TOML prints each quoted value assigned to a
# key named target, once. Over-inclusion only costs a compiler query.
rust_deps_cache_targets() {
  { grep -oE "(^|[^[:alnum:]_-])[\"']?target[\"']?[[:space:]]*=[[:space:]]*(\"[^\"]*\"|'[^']*')" "$1" ||
    [[ $? -eq 1 ]]; } | sed -E "s/^.*=[[:space:]]*[\"']//; s/[\"']\$//" | LC_ALL=C sort -u
}

# rust_deps_cache_unsupported prints why the checkout in the current directory
# configures Reindeer in a way the key cannot cover, and succeeds; it fails
# when the key covers it.
rust_deps_cache_unsupported() {
  local toml=shim/third-party/rust/reindeer.toml line
  [[ -f $toml ]] || { echo "$toml is missing"; return 0; }
  # Each names a path outside the third-party directory (fixups_dir, cargo,
  # rustc) or files whose ignore rules the key does not read
  # (gitignore_checksum_exclude).
  if line=$(grep -m1 -nE "(^|[^[:alnum:]_-])[\"']?(fixups_dir|cargo|rustc|gitignore_checksum_exclude)[\"']?[[:space:]]*=" "$toml"); then
    echo "$toml:$line sets a path the key does not cover"
    return 0
  fi
  # Without a platform table Reindeer queries the compiler for its built-in
  # platforms' targets, which the key does not list.
  if ! grep -qE "(^|[^[:alnum:]_-])[\"']?platform[\"']?[[:space:]]*[.=]" "$toml"; then
    echo "$toml defines no platform table"
    return 0
  fi
  # The target scan reads single-line strings without escapes. It also reads
  # any target assignment inside a multi-line string or a comment, which only
  # adds a compiler query to the key.
  if grep -qE "(^|[^[:alnum:]_-])[\"']?target[\"']?[[:space:]]*=[[:space:]]*(\"\"\"|''')" "$toml" ||
    [[ $(rust_deps_cache_targets "$toml") == *\\* ]]; then
    echo "$toml quotes a value in a form the target scan does not read"
    return 0
  fi
  return 1
}

# rust_deps_cache_key prints the key of the checkout in the current directory,
# or fails without printing one. Run it only when rust_deps_cache_unsupported
# fails.
rust_deps_cache_key() (
  third_party=shim/third-party/rust
  list=$(mktemp) || exit 1
  trap 'rm -f -- "$list"' EXIT
  manifest="recipe $rust_deps_cache_recipe"$'\n'
  out=$(cargo --version) || exit 1
  manifest+=$out$'\n'

  # Reindeer runs RUSTC (unset: rustc from PATH) from this directory with this
  # environment and keeps the `target_` lines of each platform's cfg, skipping
  # a query that fails. Recording the answers, not the name, covers a wrapper
  # or a toolchain that changed in place.
  rustc=${RUSTC-rustc}
  out=$("$rustc" -vV 2>&1) && status=0 || status=$?
  manifest+="rustc -vV exit $status"$'\n'$out$'\n'
  targets=$(rust_deps_cache_targets "$third_party/reindeer.toml") || exit 1
  while IFS= read -r target; do
    [[ -n $target ]] || continue
    out=$("$rustc" --print=cfg --target "$target" 2>/dev/null) && status=0 || status=$?
    manifest+="cfg $target exit $status"$'\n'
    if [[ $status -eq 0 ]]; then
      out=$({ grep '^target_' <<<"$out" || [[ $? -eq 1 ]]; }) || exit 1
      manifest+=$out$'\n'
    fi
  done <<<"$targets"

  # Fixed inputs, hashed whether or not Git tracks or ignores them.
  printf '%s\0' Cargo.lock Cargo.toml rust-toolchain.toml .gitignore shim/.gitignore \
    shim/third-party/.gitignore bootstrap/regenerate-rust-deps bootstrap/rust-deps-cache.sh \
    bootstrap/reindeer bootstrap/run-pinned-tool bootstrap/tool-pins.sh >"$list" || exit 1
  # Workspace manifests: those Git lists, and every member Cargo reports, in
  # case one is ignored.
  git ls-files -z --cached --others --exclude-standard -- ':(glob)**/Cargo.toml' >>"$list" || exit 1
  out=$(cargo metadata --locked --format-version 1 --no-deps) || exit 1
  out=$({ grep -oE '"manifest_path":"[^"]*"' <<<"$out" || [[ $? -eq 1 ]]; }) || exit 1
  here_l=$PWD/
  here_p=$(pwd -P)/
  while IFS= read -r path; do
    [[ -n $path ]] || continue
    path=${path#'"manifest_path":"'}
    path=${path%'"'}
    path=${path#"$here_p"}
    printf '%s\0' "${path#"$here_l"}" >>"$list" || exit 1
  done <<<"$out"
  # Cargo configuration and the third-party directory, through symbolic links
  # as Reindeer reads them, leaving out only what Reindeer and Cargo generate
  # there. A link loop or an unreadable directory fails the key rather than
  # leave files out of it.
  roots=("$third_party")
  [[ ! -e .cargo && ! -L .cargo ]] || roots+=(.cargo)
  find -L "${roots[@]}" \( -path "$third_party/vendor" -o -path "$third_party/BUCK" \
    -o -path "$third_party/.cargo" -o -path "$third_party/registry" -o -path "$third_party/git" \
    -o -path "$third_party/target" -o -path "$third_party/.package-cache" \) -prune \
    -o \( -type f -o -type l \) -print0 >>"$list" || {
    echo "rust-deps cache: could not list every file under ${roots[*]}" >&2
    exit 1
  }

  mapfile -d '' -t files < <(LC_ALL=C sort -zu "$list")
  for path in "${files[@]}"; do
    [[ -n $path ]] || continue
    if [[ -L $path ]]; then
      line=$(readlink -- "$path") || exit 1
      manifest+="link $path -> $line"$'\n'
    fi
    if [[ -f $path ]]; then
      line=$(sha256sum -- "$path") || exit 1
    elif [[ -L $path ]]; then
      line="dangling $path"
    else
      line="absent $path"
    fi
    manifest+=$line$'\n'
  done
  [[ $manifest == *"  Cargo.lock"$'\n'* ]] || { echo "rust-deps cache: Cargo.lock is not among the key's hashed inputs" >&2; exit 1; }
  line=$(sha256sum <<<"$manifest") || exit 1
  echo "${line%% *}"
)

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

# rust_deps_cache_retire ROOT NAME, with the exclusive lock held, renames the
# entry ROOT/NAME out of the key namespace, then removes it. A failed removal
# leaves an .evict- directory no restore reads, removed by a later publish.
rust_deps_cache_retire() {
  local root=$1 name=$2 grave
  [[ -e $root/$name || -L $root/$name ]] || return 0
  grave=$(mktemp -d "$root/.evict-$name-XXXXXX") || return 1
  if ! mv -T -- "$root/$name" "$grave/entry"; then
    rmdir -- "$grave"
    return 1
  fi
  rm -rf -- "${grave:?}" || {
    echo "warning: could not remove the retired entry $grave; the next publish retries" >&2
    return 0
  }
}

# rust_deps_cache_discard ROOT KEY removes an entry a restore could not copy, so
# this run's publish replaces it. A failure leaves it to fail, and be
# regenerated around, again.
rust_deps_cache_discard() {
  local root=$1 key=$2
  (
    exec 9>>"$root/.lock" || exit 1
    flock 9 || exit 1
    rust_deps_cache_retire "$root" "$key"
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
    # A complete entry another run published stays; anything else under the
    # key is replaced.
    if [[ ! -f $root/$key/BUCK.reindeer ]]; then
      rust_deps_cache_retire "$root" "$key" || exit 1
      mv -T -- "$staging" "$root/$key" || exit 1
    fi
    # Newest first by modification time; restore touches the entry it uses.
    old=$(find "$root" -mindepth 1 -maxdepth 1 -type d -regextype posix-extended \
      -regex '.*/[0-9a-f]{64}' -printf '%T@ %f\n' | sort -rn | tail -n +"$((keep + 1))" |
      cut -d' ' -f2-) || exit 1
    status=0
    while IFS= read -r name; do
      [[ -n $name ]] || continue
      rust_deps_cache_retire "$root" "$name" || { echo "warning: could not evict $root/$name" >&2; status=1; }
    done <<<"$old"
    # What an interrupted eviction left, and staging an interrupted publish
    # left once it is two hours old; neither is under a key.
    find "$root" -mindepth 1 -maxdepth 1 -type d -name '.evict-*' -exec rm -rf -- {} + ||
      { echo "warning: could not remove every retired entry in $root" >&2; status=1; }
    find "$root" -mindepth 1 -maxdepth 1 -type d -name '.staging-*' -mmin +120 -exec rm -rf -- {} + ||
      { echo "warning: could not remove stale staging in $root" >&2; status=1; }
    exit "$status"
  ) || echo "warning: storing the generated dependencies in $root did not complete (above); later runs may vendor again" >&2
  return 0
}
