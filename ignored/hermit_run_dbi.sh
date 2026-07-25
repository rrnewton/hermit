#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(dirname "$(realpath "$0")")"
HERMIT_DIR="$(dirname "$SCRIPT_DIR")"
HERMIT_BIN="${HERMIT_BIN:-$HERMIT_DIR/target/release/hermit}"

die() {
  printf 'hermit_run_dbi.sh: %s\n' "$*" >&2
  exit 1
}

if [[ -z "${HERMIT_DRRUN:-}" && -n "${DYNAMORIO_HOME:-}" ]]; then
  if [[ -x "$DYNAMORIO_HOME/bin64/drrun" ]]; then
    HERMIT_DRRUN="$DYNAMORIO_HOME/bin64/drrun"
  fi
fi

if [[ -z "${HERMIT_DRRUN:-}" && -d "$HERMIT_DIR/target/release/build" ]]; then
  HERMIT_DRRUN="$(
    find "$HERMIT_DIR/target/release/build" \
      -path '*/out/dynamorio-install/bin64/drrun' -type f -printf '%T@\t%p\n' 2>/dev/null |
      sort -nr | head -n1 | cut -f2-
  )"
fi

if [[ -z "${HERMIT_DRRUN:-}" && -x "$HOME/dynamorio/install/bin64/drrun" ]]; then
  HERMIT_DRRUN="$HOME/dynamorio/install/bin64/drrun"
fi

[[ -n "${HERMIT_DRRUN:-}" ]] || die "could not find drrun in the release build tree or $HOME/dynamorio/install"
[[ -x "$HERMIT_DRRUN" ]] || die "drrun is not executable: $HERMIT_DRRUN"

if [[ -z "${DYNAMORIO_HOME:-}" ]]; then
  DYNAMORIO_HOME="$(dirname "$(dirname "$HERMIT_DRRUN")")"
fi
[[ -d "$DYNAMORIO_HOME" ]] || die "DynamoRIO home is not a directory: $DYNAMORIO_HOME"

if [[ -z "${HERMIT_DBI_CLIENT:-}" && -d "$HERMIT_DIR/target" ]]; then
  HERMIT_DBI_CLIENT="$(
    find "$HERMIT_DIR/target" \
      -name libreverie_dbi_client.so \
      -type f -printf '%T@\t%p\n' 2>/dev/null |
      sort -nr | head -n1 | cut -f2-
  )"
fi

if [[ -z "${HERMIT_DBI_CLIENT:-}" ]]; then
  for candidate in \
    "$HOME/work/dev-hermit/reverie/target/release/reverie-dbi-native/libreverie_dbi_client.so" \
    "$HOME/work/dev-reverie/reverie/target/reverie-dbi-native/libreverie_dbi_client.so" \
    "$HOME/work/dev-reverie/reverie/target/release/reverie-dbi-native/libreverie_dbi_client.so"
  do
    if [[ -f "$candidate" ]]; then
      HERMIT_DBI_CLIENT="$candidate"
      break
    fi
  done
fi

[[ -n "${HERMIT_DBI_CLIENT:-}" ]] || die "could not find libreverie_dbi_client.so under target/ or a Reverie build tree"
[[ -f "$HERMIT_DBI_CLIENT" ]] || die "DBI client is not a file: $HERMIT_DBI_CLIENT"
[[ -x "$HERMIT_BIN" ]] || die "release Hermit binary is not executable: $HERMIT_BIN"

export DYNAMORIO_HOME
export HERMIT_DRRUN
export HERMIT_DBI_CLIENT

exec "$HERMIT_BIN" --backend dbi "$@"
