#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# prelude-cache-key.sh — keep rust-script consumers rebuilding when ANY
# `#[path]`-included module changes.
#
# SCOPE WIDENED 2026-08-08. This file was written for the shared prelude, which
# was then the only `#[path]` module any consumer had. scripts/validate.rs now
# includes SEVEN more (scripts/lib/validate_*.rs) and they were not covered, so
# the key attested one input out of eight. Measured on validate.rs at
# 0f90722a6, with `validate_runtime::self_test` mutated to return `Err`:
#
#   warm cache, mutated tree   -> --self-test EXIT 0, binary mtime UNCHANGED,
#                                 the planted string absent from the output
#   same mutated tree, binary deleted -> EXIT 2, "SELF-TEST FAILED: MUTANT"
#
# Only the cache differed. So the driver's own self-test can report GREEN over
# source it never compiled — "we did not check" rendered as "we checked and it
# was green", inside the checks meant to certify the driver. The name is kept
# because every consumer's stamped line and the sigpipe gate reference this path;
# the mechanism is now general.
#
# THE TRAP: rust-script (0.36) decides a cached binary is fresh by comparing the
# built binary's mtime against ONLY the main script file's mtime (and the
# generated Cargo manifest's). It never looks at `#[path = "..."]`-included
# modules. Our standalone scripts all `#[path]`-include
# scripts/lib/rust_script_prelude.rs, so editing the prelude does NOT bust their
# caches: a machine whose ~/.cache/rust-script warmed before the change keeps
# running the pre-change binary (e.g. `manifest-cli list | head` exits 141 even
# though the SIGPIPE handler is present in prelude source).
#
# THE FIX: stamp a short digest of the prelude onto each consumer's
# `mod rust_script_prelude;` line. Because the digest lives in the consumer's
# OWN bytes, any prelude change is accompanied by a consumer content change,
# which (a) propagates through git as a fresh mtime + new content on checkout and
# (b) is exactly what rust-script's freshness check consults — so the consumer
# rebuilds and picks up the new prelude on its next run.
#
# Usage:
#   scripts/lib/prelude-cache-key.sh            # --check (default): fail if stale
#   scripts/lib/prelude-cache-key.sh --check
#   scripts/lib/prelude-cache-key.sh --check-runtime
#   scripts/lib/prelude-cache-key.sh --write    # restamp all consumers
#
# Run --write after editing scripts/lib/rust_script_prelude.rs. The --check mode
# is wired into scripts/check-script-sigpipe.sh so a forgotten restamp is caught.
# --check-runtime additionally resolves the exact release binary rust-script
# would execute for every consumer and fails if that binary predates the main
# script, generated Cargo manifest, or shared prelude. It is the observable
# agent-side predicate: its FRESH lines name the digest, binary, and timestamps.
#
# Effective invalidation mechanisms:
#   rust-script --force SCRIPT [SCRIPT_ARGS...]  # rebuild one script now
#   rust-script --clear-cache                    # remove all cached binaries
#   XDG_CACHE_HOME="$(mktemp -d)" SCRIPT ...     # execute with a cold cache
# `--write` is the repository mechanism: it changes every consumer's own bytes,
# so its next normal invocation is newer than the cached binary and rebuilds.
#
# This is a bash script, not a rust-script, on purpose: it maintains the very
# cache key the rust-script consumers depend on, so it must run correctly even
# when their caches are stale (no bootstrap-through-the-bug).
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

PRELUDE="scripts/lib/rust_script_prelude.rs"
MARKER="mod rust_script_prelude;"
TAG="rust-script cache-key:"

[[ -f $PRELUDE ]] || { echo "prelude-cache-key.sh: missing $PRELUDE" >&2; exit 2; }
command -v sha1sum >/dev/null 2>&1 || { echo "prelude-cache-key.sh: sha1sum required" >&2; exit 2; }

# EVERY `#[path]`-included module of a consumer, resolved relative to the
# including file, sorted for a stable digest. rust-script ignores all of them
# equally -- the prelude was simply the first one we were bitten by -- so the key
# must cover the whole set or it certifies only part of the source it compiles.
included_modules() {
  local f=$1 dir rel
  dir="$(dirname "$f")"
  grep -oE '#\[path = "[^"]+\.rs"\]' "$f" \
    | sed -E 's/#\[path = "([^"]+)"\]/\1/' \
    | while IFS= read -r rel; do printf '%s\n' "$dir/$rel"; done \
    | sort -u
}

# Digest over the CONTENT of every included module. Hashing the per-file hashes
# (rather than the concatenated bytes) keeps the result independent of file order
# and immune to content shifting across a module boundary.
digest_for() {
  local f=$1 mods
  mapfile -t mods < <(included_modules "$f")
  if [[ ${#mods[@]} -eq 0 ]]; then
    echo "prelude-cache-key.sh: $f includes no #[path] module" >&2
    return 2
  fi
  local m
  for m in "${mods[@]}"; do
    [[ -f $m ]] || { echo "prelude-cache-key.sh: $f includes missing module $m" >&2; return 2; }
  done
  sha1sum "${mods[@]}" | sha1sum | cut -c1-12
}

# A consumer is a rust-script (shebang `#!/usr/bin/env rust-script`) that
# `#[path]`-includes the prelude as a module. Discovered dynamically so a new
# consumer is covered automatically. The prelude itself and the rustc-compiled
# sigpipe_smoke fixture (no rust-script cache) are excluded.
consumers() {
  local f
  while IFS= read -r f; do
    [[ $f == "$PRELUDE" ]] && continue
    [[ $(head -1 "$f") == "#!/usr/bin/env rust-script" ]] || continue
    printf '%s\n' "$f"
  done < <(grep -rl --include='*.rs' -F "$MARKER" scripts tests 2>/dev/null | sort)
}

stamped_line() { printf '%s // %s %s (regen: scripts/lib/prelude-cache-key.sh --write)' "$MARKER" "$TAG" "$1"; }

mode="${1:---check}"
case "$mode" in
  --write|--check|--check-runtime) ;;
  *)
    echo "Usage: $0 [--check | --check-runtime | --write]" >&2
    exit 2
    ;;
esac

mapfile -t files < <(consumers)
[[ ${#files[@]} -gt 0 ]] || { echo "prelude-cache-key.sh: no rust-script consumers found (looked for '$MARKER')" >&2; exit 2; }

if [[ $mode == "--write" ]]; then
  changed=0
  for f in "${files[@]}"; do
    want="$(digest_for "$f")"
    new_line="$(stamped_line "$want")"
    # Replace the whole marker line (bare or previously stamped) in place.
    tmp="$(mktemp)"
    awk -v marker="$MARKER" -v repl="$new_line" '
      index($0, marker) == 1 { print repl; next }
      { print }
    ' "$f" > "$tmp"
    if ! cmp -s "$f" "$tmp"; then cat "$tmp" > "$f"; changed=$((changed+1)); echo "stamped $f -> $want"; fi
    rm -f "$tmp"
  done
  echo "prelude-cache-key.sh: OK — ${#files[@]} consumer(s), $changed updated"
  exit 0
fi

# --check
stale=()
for f in "${files[@]}"; do
  want="$(digest_for "$f")"
  have="$(awk -v marker="$MARKER" -v tag="$TAG" '
    index($0, marker) == 1 {
      i = index($0, tag)
      if (i == 0) { print "MISSING"; exit }
      rest = substr($0, i + length(tag))
      n = split(rest, a, " ")
      for (k = 1; k <= n; k++) if (a[k] != "") { print a[k]; exit }
      print "MISSING"; exit
    }
  ' "$f")"
  [[ $have == "$want" ]] || stale+=("$f (have: ${have:-none}, want: $want)")
done

if [[ ${#stale[@]} -gt 0 ]]; then
  {
    echo "prelude-cache-key.sh: FAIL — module cache-key is stale in ${#stale[@]} consumer(s):"
    printf '  %s\n' "${stale[@]}"
    echo "  A #[path]-included module changed but consumers were not restamped, so"
    echo "  rust-script would serve stale cached binaries on warm-cache machines."
    echo "  Fix: scripts/lib/prelude-cache-key.sh --write   (then commit the result)"
  } >&2
  exit 1
fi
echo "prelude-cache-key.sh: OK — ${#files[@]} consumer(s) carry a current module cache-key"

[[ $mode == "--check-runtime" ]] || exit 0

# Mirror rust-script 0.36's Linux cache root. `dirs::cache_dir()` uses
# XDG_CACHE_HOME when set and otherwise HOME/.cache. Authoritative Hermit tools
# are invoked through their shebang and therefore use the release cache.
command -v rust-script >/dev/null 2>&1 || {
  echo "prelude-cache-key.sh: rust-script is required for --check-runtime" >&2
  exit 2
}
command -v stat >/dev/null 2>&1 || {
  echo "prelude-cache-key.sh: stat is required for --check-runtime" >&2
  exit 2
}

cache_home="${XDG_CACHE_HOME:-${HOME:?HOME must be set}/.cache}"
binary_dir="$cache_home/rust-script/binaries/release"

# rust-script compares the cached binary's creation time (falling back to its
# mtime) with the main script and generated manifest. We additionally compare
# with the shared prelude: that is the input rust-script itself ignores.
file_mtime() { stat -c '%y' "$1"; }
binary_build_time() {
  local birth
  birth="$(stat -c '%w' "$1")"
  if [[ $birth == '-' ]]; then file_mtime "$1"; else printf '%s\n' "$birth"; fi
}

fresh=0
cold=0
runtime_stale=0
for f in "${files[@]}"; do
  want="$(digest_for "$f")"
  project="$(rust-script --package "$f")"
  manifest="$project/Cargo.toml"
  [[ -f $manifest ]] || {
    echo "prelude-cache-key.sh: FAIL — rust-script did not generate $manifest for $f" >&2
    runtime_stale=$((runtime_stale + 1))
    continue
  }
  bin_name="$(awk '
    $0 == "[[bin]]" { in_bin = 1; next }
    in_bin && $1 == "name" && $2 == "=" {
      gsub(/"/, "", $3); print $3; exit
    }
  ' "$manifest")"
  [[ -n $bin_name ]] || {
    echo "prelude-cache-key.sh: FAIL — cannot read [[bin]].name from $manifest" >&2
    runtime_stale=$((runtime_stale + 1))
    continue
  }
  binary="$binary_dir/$bin_name"
  if [[ ! -f $binary ]]; then
    echo "COLD  $f key=$want binary=$binary (absent; next invocation must compile)"
    cold=$((cold + 1))
    continue
  fi

  built="$(binary_build_time "$binary")"
  consumer_time="$(file_mtime "$f")"
  manifest_time="$(file_mtime "$manifest")"
  newest_input="$consumer_time"
  [[ $manifest_time > $newest_input ]] && newest_input="$manifest_time"
  # EVERY included module counts, not just the prelude: each one is an input
  # rust-script's own freshness check ignores.
  newest_module=""
  newest_module_time=""
  while IFS= read -r m; do
    mt="$(file_mtime "$m")"
    if [[ -z $newest_module_time || $mt > $newest_module_time ]]; then
      newest_module_time="$mt"
      newest_module="$m"
    fi
  done < <(included_modules "$f")
  [[ -n $newest_module_time && $newest_module_time > $newest_input ]] && newest_input="$newest_module_time"

  if [[ $built < $consumer_time || $built < $manifest_time || $built < $newest_module_time ]]; then
    {
      echo "STALE $f key=$want"
      echo "      binary=$binary"
      echo "      built=$built newest-input=$newest_input"
      echo "      consumer=$consumer_time manifest=$manifest_time"
      echo "      newest-module=$newest_module ($newest_module_time)"
    } >&2
    runtime_stale=$((runtime_stale + 1))
  else
    echo "FRESH $f key=$want binary=$binary built=$built newest-input=$newest_input"
    fresh=$((fresh + 1))
  fi
done

echo "prelude-cache-key.sh: runtime summary — fresh=$fresh cold=$cold stale=$runtime_stale cache=$binary_dir"
if (( runtime_stale > 0 )); then
  {
    echo "prelude-cache-key.sh: FAIL — $runtime_stale cached executable(s) can run stale prelude code."
    echo "  Rebuild one: rust-script --force SCRIPT [SCRIPT_ARGS...]"
    echo "  Invalidate all: rust-script --clear-cache"
    # The command substitution is intentionally printed for the agent to run.
    # shellcheck disable=SC2016
    echo '  Prove cold: XDG_CACHE_HOME="$(mktemp -d)" SCRIPT [SCRIPT_ARGS...]'
    echo "  Then rerun: scripts/lib/prelude-cache-key.sh --check-runtime"
  } >&2
  exit 1
fi
